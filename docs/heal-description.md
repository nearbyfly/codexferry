# Heal 子系统档案

> 本文件是 codexferry heal 子系统的**当前状态**参考（不是设计 spec，
> 不是实施计划）。设计与变更意图请看 `docs/superpowers/specs/` 下
> `*-design.md`；本文件反映**已落地的代码**，与源码同步更新。

## 1. 定位与术语

**heal 子系统**是 codexferry daemon 在 LLM 上游（upstream）返回内容
上做的一层**就地修复**：

- 修复对象：上游（LLM 侧）响应的文本/事件序列/流结束形态偏离 Responses 契约的部分。
- 不修复对象：客户端（Codex CLI）的请求。
- 触发姿态：「按 anomaly 触发，健康流零开销；上游修好后自动失活」。
  历史上以「class-B repair」自指（参见 `src/heal/mod.rs` 模块级 doc）。
- 命名约定：在本仓库内，「**上游（upstream）**」一律指 LLM API 提供方
  （DeepSeek、Kimi、GLM、MiniMax、SiliconFlow、…）；「**客户端（client）**」/
  「**Codex CLI**」一律指本机跑的 codex 客户端。证据见代码：
  `send_upstream`、`upstream_resp`、`upstream_id`、`upstream_model`、
  `upstream_started` 都是 daemon 看出去（去 LLM）的方向。

## 2. 总览矩阵

按 **方向 × 子类** 划分全部已注册 quirk（`src/quirks.rs::QUIRK_NAMES`）：

```
REQUEST 侧（构造请求时改）
  └─ glm_thinking           chat only —— 给上游 GLM 请求加 thinking 字段

RESPONSE 侧（修复上游响应）
  ├─ 内容修复（content heal）
  │   ├─ dsml_heal          chat + responses —— 文本里漏 DSML markup → 结构化 tool call
  │   └─ think_tags         chat + responses —— 文本里漏 think 标签 → 推到 reasoning 通道
  ├─ 流结束判定（end-of-stream rescue）
  │   └─ missing_done       chat only —— 无 [DONE] 但有 finish_reason → 视为完成
  └─ 流形状修复（stream-shape heal）
      └─ merge_fragmented   responses only —— 上游非合规流折回合规
```

## 3. 总览表（按 quirk 排）

| Quirk | 方向 | 子类 | 触发对象 | 默认 | 进 HealGates? |
|---|---|---|---|---|---|
| `glm_thinking` | request | 请求构造 | 上游 LLM（GLM/Zhipu/bigmodel） | ON | **否**（独立读 config） |
| `dsml_heal` | response | 内容修复 | 上游 LLM 响应文本里的 DSML markup | ON | **是**（`HealGates.dsml`） |
| `think_tags` | response | 内容修复 | 上游 LLM 响应文本里的 think 标签 | ON | **是**（`HealGates.think`） |
| `missing_done` | response | 流结束判定 | 上游 LLM 响应流结束但无 `[DONE]` | ON | **否**（独立读 config） |
| `merge_fragmented` | response | 流形状修复 | 上游 LLM 响应里连续同类型 `output_item.added` | ON | **是**（`HealGates.merge_fragmented`） |

## 4. HealGates 接口

```rust
// src/heal/mod.rs
pub struct HealGates {
    pub dsml: bool,    // 默认 true（quirk `dsml_heal`）
    pub think: bool,   // 默认 true（quirk `think_tags`）
    pub merge_fragmented: bool, // 默认 true（quirk `merge_fragmented`）
}
```

- 默认三字段全 ON（`Default for HealGates` 显式实现，注释解释：deny-by-default
  会静默关掉所有修复，与设计意图相反）。
- 由 daemon 在每次请求**一次性读 config** 后构造，传给具体 handler /
  converter / healer。`HealGates` 不是 hot-reload 的载体 —— 它只是当前
  config 的快照；hot-reload 是 watcher → channel → applier 那一层做的。
- `glm_thinking` 和 `missing_done` **不进 HealGates**，理由：分别跨
  「request」与「end-of-stream」两个子类，与 content / stream-shape 这
  两类响应修复不在同一抽象层。它们各自的 handler 直接调
  `config.quirk_enabled("glm_thinking")` /
  `config.quirk_enabled("missing_done")`。

## 5. 杀开关（kill switch）

```toml
[quirks]
disabled = ["dsml_heal", "think_tags", "glm_thinking", "missing_done"]
```

- `Config::quirk_enabled(name)`（`src/config.rs:565`）：大小写不敏感匹配
  `disabled` 列表；命中即 `false`，否则 `true`。
- `quirks::unknown_quirk_names(&disabled)`（`src/quirks.rs:55`）：过滤
  未在 `QUIRK_NAMES` 注册的名字，返回原拼写（保留 warn 时给用户看到
  自己写的东西），配置 validation 时 warn 不 reject。
- 热重载：`notify` watcher 通过 unbounded channel 把新 config 发给
  applier 任务，**下一个请求**就拿到新 gate（见 `proxy/chat.rs:39` 注释
  与 `docs/superpowers/specs/2026-08-28-hot-reload-watcher-fix-design.md`）。

## 6. 各 quirk 详解

### 6.1 `glm_thinking` —— request 侧构造

| 字段 | 值 |
|---|---|
| 方向 | request |
| 子类 | 请求构造 |
| 适用路径 | chat only（responses 路径透传不构造请求） |
| 触发条件 | 模型名匹配 `quirks::is_glm_like_model`（子串匹配 `glm` / `zhipu` / `bigmodel`）**且** quirk 开关 ON |
| 修复做法 | `ChatRequest.thinking` 字段填 `ChatThinking::enabled()`，让 GLM 显式开思考 |
| Hook 点 | `src/convert/request.rs:259`（构造处） |
| 配置读取 | `src/proxy/chat.rs:42-53` 一次性读 config |
| 状态位 | 不在 `HealGates`，独立 bool |

### 6.2 `dsml_heal` —— response 侧内容修复

| 字段 | 值 |
|---|---|
| 方向 | response |
| 子类 | 内容修复 |
| 适用路径 | **chat + responses** |
| 触发条件 | 任意 delta / 完整响应文本含 DSML markup（`<ml>` / `</ml>` / `◁ml▷`）|
| 修复做法 | 两阶段：① `DsmlStreamFilter` 把 `<ml>…</ml>` 或 `◁ml▷…◁/ml▷`（Kimi 变体）+ 对应闭合 |
| 状态位 | `HealGates::dsml: bool`，默认 `true` |
| Hook 点 | chat：`src/proxy/chat.rs` → `StreamConverter::new` 构造 `dsml_filter`（4 处）；responses：`src/proxy/passthrough.rs` → `ResponsesStreamHealer::new` 构造 `dsml`（两处：流 + 非流） |
| 顺序 | DSML 隔离在第一阶段（`dsml.rs` 顶层 doc 解释：think 标签可能被合法嵌入 DSML 参数值里，所以必须先剥 DSML 再剥 think）|

dsml 解析细节：
- 模块：`src/heal/dsml.rs`（677 行）+ `dsml_tests.rs`（386 行）。
- `DsmlStreamFilter::push(delta)` 在每条 delta 上做最长前缀 withholding
  —— `longest_tag_prefix_suffix()`，保证跨 chunk 的 marker
  （`<m` 之后跟 `l>`）也能识别。
- 未闭合的 DSML 块在 `finish()` 时 flush tail（不丢）。
- `heal_dsml_chat_message`（非流）把 markup 内容追加到 `tool_calls` 末尾
  （作为 `「DSML 逃逸」call`），而不是直接改文本（text 里塞 markup 是
  Codex 客户端的问题，daemon 负责修复）。

### 6.3 `think_tags` —— response 侧内容修复

| 字段 | 值 |
|---|---|
| 方向 | response |
| 子类 | 内容修复 |
| 适用路径 | **chat + responses** |
| 触发条件 | 任意 delta / 完整响应文本含 think markup（`<think>…</think>` / `◁think▷`（Kimi 变体）+ 对应闭合 |
| 修复做法 | 把 `<think>…</think>` 内容从可见文本移到 reasoning 通道 |
| 状态位 | `HealGates::think: bool`，默认 `true` |
| Hook 点 | 与 `dsml_heal` 完全相同的 4 处；作为两阶段管道的第二阶段 |
| 顺序 | **DSML 隔离第一 → think 拆分第二**（`heal/mod.rs` 顶层 doc 解释：DSML 参数值里可能合法包含 `<think>` 文本，所以必须先剥 DSML 再剥 think） |

think 解析细节：
- 模块：`src/heal/think.rs`（192 行）+ `think_tests.rs`（152 行）。
- `ThinkStreamFilter::push(delta)` 在每条 delta 上做最长前缀 withholding
  —— `longest_tag_prefix_suffix()`，保证跨 chunk 的 marker
  （`<thi` 之后跟 `nk>`）也能识别。
- 未闭合的 think 块在 `finish()` 时视为 reasoning（不丢）。

### 6.4 `missing_done` —— response 侧流结束判定

| 字段 | 值 |
|---|---|
| 方向 | response |
| 子类 | 流结束判定 |
| 适用路径 | **chat only**（responses 路径以 SSE 闭合为准，不读 `[DONE]`） |
| 触发条件 | 流自然结束（chunk 用尽）**且** 没收到 `[DONE]` 哨兵 **且** 有 chunk 携带 `finish_reason` **且** **不**是 idle timeout **且** **不**是 client 断开 |
| 修复做法 | 调 `converter.finish()` 出完成序列，并把这一支 warn 打到 `tracing::warn!` |
| Hook 点 | `src/proxy/chat.rs:182-302`（chat 流任务循环里） |
| 状态位 | 不在 `HealGates`，独立 bool |

与其他 end-of-stream 路径的区分：
- `saw_done == true`：默认路径，调 `converter.finish()`，无 warn。
- `saw_done == false && finish_reason.is_some() && quirk on`：quark 救
  一支，打 `quirk missing_done fired` warn。
- `saw_done == false && finish_reason.is_some() && quirk off`：落到
  `on_error` 路径，发 `response.failed`。
- `saw_done == false && finish_reason.is_none()`：truncated stream，落
  到 `on_error`（不救）。
- `timed_out == true`（idle timeout）：永远不救，已在循环里打过自己的 warn。
- `client_disconnected == true`：永远不救，避免把客户端断连误归为上游异常。

### 6.5 `merge_fragmented` —— response 侧流形状修复

| 字段 | 值 |
|---|---|
| 方向 | response |
| 子类 | 流形状修复（与内容修复正交）|
| 适用路径 | **responses only**（chat 是构造者姿态，天然不碎片）|
| 触发条件 | 连续同类型 `output_item.added`：`message` / `reasoning` 同 type 直接合并；`function_call` 额外要求 `call_id` 匹配（OpenAI 契约：同 call 必须同 item）|
| 修复做法 | 把 N 个连续同类型 item 折回 1 个：第 1 个原样透传、后续 N-1 个 added 抑制、deltas 重写 `item_id`/`output_index` 为首碎片、累积 merged_* 文本、run 末尾 flush 合成 `content_part.done` + `output_item.done` |
| 状态位 | `HealGates::merge_fragmented: bool`，默认 `true` |
| Hook 点 | `src/proxy/passthrough.rs` 的 healed 分支，**插在 `ResponsesStreamHealer` 前面** |
| 与 healer 的关系 | D-1：merger 只管形状，healer 只管内容。merger 改写后 healer 看到「真的只有一个 item」——**消除了 review #5/#7 的 multi-item 边界** |

模块：`src/heal/merge.rs` + `src/heal/merge_tests.rs`（19 个 fixture，按 M1–M9 / W1–W5 / E1–E4 / S1–S3 / K1 编号）。

设计 spec：`docs/superpowers/specs/2026-08-30-fragmented-items-merger-design.md`。
NOTES 调查：`NOTES-2026-08-28-minimax-m3-fragmentation.md`（git-exclude，仅本地）。

## 7. heal 模块目录结构

```
src/heal/                          共 3584 行（含测试）
├── mod.rs                          63   HealGates 门面 + 默认全 ON
├── dsml.rs                        677   DsmlStreamFilter + heal_dsml_chat_message
├── dsml_tests.rs                  386   + parse_leaked_tool_calls + 双 dialect
├── think.rs                       192   ThinkStreamFilter + heal_think_chat_message
├── think_tests.rs                 152   + contains_think_markup
├── merge.rs                       463   FragmentedItemMerger + ItemType + RunState
├── merge_tests.rs                 544   M1–M9 / W1–W5 / E1–E4 / S1–S3 / K1 fixtures
├── responses.rs                   611   ResponsesStreamHealer（流式）
└── responses_healer_tests.rs      496   + heal_responses_body（非流式）
                                    + INJECT_INDEX_BASE = 10_000
```

`quirks.rs`（顶层 src/，63 行）放 `QUIRK_NAMES` 注册表与
`is_glm_like_model`，不属于 `heal/` 模块，但提供注册与模型名匹配。

## 8. 两个 state machine 字段对照

`StreamConverter`（chat，构造者姿态）与 `ResponsesStreamHealer`（responses，
观察者+改写者姿态）与 `FragmentedItemMerger`（responses，流形状修复）的
对照 —— 字段名故意保持相似，以便看出对应关系：

| 字段 | StreamConverter（chat） | ResponsesStreamHealer（responses） | FragmentedItemMerger（responses, stream-shape） |
|---|---|---|---|
| 持有 filter | `dsml_filter`, `think_filter` | `dsml`, `think` | — |
| 文本累积 | `acc.text`, `acc.reasoning` | `healed_text`, `reasoning_text` | `merged_text`, `merged_reasoning`, `merged_arguments` |
| 跟踪/状态 | `message_output_index`, `reasoning_output_index` | `message_item_id`, `message_output_index` | `run: Option<RunState { item_type, start_idx, start_id, call_id, merged_*, part_added_emitted }>` |
| 下一个分配 index | `next_output_index: usize` | `next_index: usize`（from `INJECT_INDEX_BASE`） | —（不分配新 index；改写现有 index） |
| tool call 累积 | `tool_calls: BTreeMap<...>` | `injected_calls: Vec<...>` | —（call_id 匹配，不累积） |
| finish 幂等 | `finish_emitted: bool` | `finished: bool` | γ-1: `finish()` 不合成 done |

`HealGates` 在三个 state machine 里的接线方式：
- chat：`StreamConverter::new(response_id, model, heal, namespace_tools)`
  把 `heal` 整体传入，converter 内部用 `heal.dsml`/`heal.think` 构造 filter。
- responses：`ResponsesStreamHealer::new(gates: HealGates)` 同样把 `heal`
  整体传入。
- responses（形状）：`FragmentedItemMerger::new(heal.merge_fragmented)`
  只取 `merge_fragmented` 字段，插在 healed 分支里。

disabled 状态下：
- `dsml_filter`/`think_filter` 仍然是真 filter 实例，但 `enabled=false`，
  `push()` 直接返回原文本 / 把 delta 全部当 text 输出（`think.rs:48`、
  `dsml.rs:567`）。
- `FragmentedItemMerger` 在 `enabled=false` 时是 identity（直接透传
  所有事件，不改动）。

## 9. chat 路径的 in-place 修复（非流式）

chat 非流式不走 filter —— 直接在 `choice.message` 上调阻塞函数：

```rust
// src/proxy/chat.rs:473-485
if let Some(choice) = chat_resp.choices.first_mut() {
    if heal.dsml {
        crate::heal::heal_dsml_chat_message(&mut choice.message);
    }
    if heal.think {
        crate::heal::heal_think_chat_message(&mut choice.message);
    }
}
```

`heal_dsml_chat_message` 原地改 `content`（剥 markup → 清干净文本）和
`tool_calls`（追加结构化 call）。`heal_think_chat_message` 原地改
`content` 与 `reasoning_content`。

## 10. responses 路径的 in-place 修复（非流式）

`heal_responses_body(&[u8], gates: HealGates) -> Vec<u8>`（`src/heal/responses.rs:504`）：

- 双 quirk 都关时直接返回原 body 字节。
- 对 `response.output[]`（native OpenAI 嵌套）或顶层（个别 provider 扁
  平结构）下每个 `message` item 的每个 `output_text` part 跑两阶段管道
 （DSML → think）。`healed_calls` 推到尾部，think reasoning item 插入到
 所属 message 之前（**canonical 顺序**，见 §11）。
- 修改后的 JSON 重新序列化。`output` 数组在序列化时按插入顺序保持。

## 11. session capture 一致性

session 存储依赖「健康流」判断 + capture 的 items 列表：

- **chat 路径**：`OutputAccumulator.items` 是 `StreamConverter` 自己构造
  的（构造者姿态），所以已经是规范顺序：`reasoning` → `message` →
  `function_call[]`（`chat_response_to_items` 文档明确写）。session
  replay 把这数组整体存进 `SessionStore`。
- **responses 路径**：`completed_capture(output)`（`src/proxy/capture.rs`）
  从 `last_completed_payload` 解码出来的 `response.output` 里**取 items**。
  healing 改写 `response.completed` 的 `output` 数组是**就地改写** —— 修好
  的 items 进 session，未修的也是同一份。这保证了 session replay 与 client
  看到的 wire 一致。

canonical turn 顺序：reasoning 在 message 之前，function_call 在 message
之后。`insert_at = output.iter().position(|i| i["type"] == "message").unwrap_or(output.len())`
—— reasoning 永远插到第一个 message 之前，function_call 永远 append 末尾。
这个顺序与 chat 路径 `chat_response_to_items` 的输出顺序一致，session
replay 跨路径不会漂移。

## 12. 「框架统一 / 代码两套」总结

**统一的部分**（一处抽象跨两路径）：

- `HealGates` 抽象（dsml + think + merge_fragmented 三字段，所有 chat/responses
  的 content heal 和 stream-shape heal 都从这过）。
- `QUIRK_NAMES` 注册 + `[quirks] disabled` 杀开关 + 「trigger 自激活、
  健康流零开销」姿态。
- session capture 模型（chat `OutputAccumulator.items` / responses
  `completed_capture(output)` 都是「先看健康流、再决定覆盖」）。
- `INJECT_INDEX_BASE = 10_000`（注入 item 不会撞真实 output_index）。

**不统一的部分**（必要的代码层差异）：

- 实际 filter 代码两套：chat 持 `DsmlStreamFilter` / `ThinkStreamFilter`
  + 自己造 events；responses 持同名 filter + 用 `sse_block()` 重写事件。
  必要原因 —— 输入格式不同（Chat delta vs Responses SSE event）。
- `glm_thinking` 仅 chat（请求构造，不属于响应修复）。
- `missing_done` 仅 chat（chat 才有 `[DONE]` 哨兵语义）。
- **stream-shape heal 在 responses 上由 `merge_fragmented` 提供**（新增类别）
  —— chat 仍是「按规范构造」，`message_output_index: Option<usize>`
  整个 turn 至多 None→One 一次是不变量；responses 由 `FragmentedItemMerger`
  把上游非合规流（每个 chunk 一个 item）折回合规（一个逻辑 item 一个
  add/done 周期）。

## 13. 已知 TODO 与边界

`src/heal/responses.rs` 里有几条明确标了 `TODO(phase-b review #N)` 的
已知边界，全部围绕「multi-message stream」与「上游 echo 与 deltas 不一致」：

- review #5：`rewrite_text_echo` 没做 `item_id` guard。
- review #7：`insert_at` 选第一个 message，不是 tracked 那个；multi-
  message stream 时 reasoning 会插到 untracked message 之前。
- review #8：withheld tail 在 `response.completed` 时 flush —— 顺序与
  chat path 的「flush-before-done」相反。经典泄漏是空 tail，不触发；
  若出现中段标签的泄漏需要重审。
- review #9：`heal_responses_body` 的 `fired` 只在 reasoning 非空时设，
  空 think 块 (`<think>
</think>

`) 被剥但 `fired` 仍 false，函数会
  返回原字节，markup 留在 wire 上。
- review #11：`rewrite_completed` 的 think warn 键是 `reasoning.is_some()`，
  在 flush tail 里才出现的 think 不会 warn（telemetry only，不影响 wire）。
- review #12：未见到 `output_item.added` 时 flush tail 的 `item_id` 默认
  为 `""`、`output_index` 为 0 —— 严格 Responses client 会拒。

**当前所有 TODO 都是 multi-message / 罕见上游变形 触发的边界**，
**不是日常路径**。本次 heal 子系统**不试图一次性修干净**（spec 边界
之外），后续若 multi-message stream 真的被观测到，再单独提 spec。

> **注意**：`merge_fragmented` 的落地（2026-08-30）消除了 review #5/#7 的
> multi-item 边界：merger 把 N 个碎片 item 折回 1 个后，healer 看到的
> 是合规的单个 item，不再触发那两个 review 的边界条件。

## 14. 后续扩展点（已落地：`merge_fragmented`）

`merge_fragmented` quirk 已落地（2026-08-30，见 spec
`docs/superpowers/specs/2026-08-30-fragmented-items-merger-design.md`）。
模块 `src/heal/merge.rs`，接入 `src/proxy/passthrough.rs` 的 healed 分支。

未来若再加新响应侧修复：
- 设计 spec：`docs/superpowers/specs/<date>-<topic>-design.md`
- 本档案同步更新：§3 加行、§6 加小节、§7 加模块目录行、§8 加 state machine 对照列、§12 更新总结措辞
