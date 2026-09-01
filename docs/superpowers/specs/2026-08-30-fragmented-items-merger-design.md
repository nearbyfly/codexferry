# Design: Fragmented-Items Merger — heal pass for MiniMax M3 Responses streaming

**Date:** 2026-08-30
**Status:** approved for implementation
**Base:** main @ `191c9b6`
**Branch:** main
**Related:**
- `NOTES-2026-08-28-minimax-m3-fragmentation.md` (git-exclude, 本地调查；设计草案源)
- `docs/heal-description.md` §14 (扩展点占位；spec 落地后据此更新)
- `docs/superpowers/specs/2026-08-28-hot-reload-watcher-fix-design.md` (style 参考：HEAL 模块拆分时遵循的命名 / 接入约定)

## Problem

MiniMax M3 Responses 网关在「裸聊天形态」（无 `instructions` 且无 `tools`）下，每个
streaming chunk 发成一个独立 output item（NOTES §2 实测：典型 5–14 items，
数量随 chunk 划分随机波动）。Codex TUI 逐 item 渲染
（`codex-rs/core/src/stream_events_utils.rs:288` 的
`handle_output_item_done`），所以一个本应连续的助手回复被切成多个 `•`
bullet，断点在句子中间（`Hey there` → `! How's it going? What are we
diving` → `into today?`）。`response.completed` 的 `output` 数组同样带
全部碎片，session 回放也会碎。

**两个 Responses 网关序列化器**（NOTES §2 已确认机理）：

- **裸聊天形态**（无 `instructions` 且无 `tools`）→ 坏序列化器：按 chunk 拆 item
- **带 `instructions`（或 `tools`）的 agentic 形态**（= 所有真实 codex 请求）→
  合规序列化器：单 item + delta 流式

实测只观测到 `message` / `reasoning` 被碎片化。`function_call` 拆 ID 在
NOTES 范围内未观测，但 OpenAI Responses 契约明确「同 call 必须同 item」
（`response.function_call_arguments.delta` 必须配同一 `item_id`），拆了就是
契约违反 —— **顺手覆盖**。

`dsml_heal` / `think_tags` / `glm_thinking` / `missing_done` 都不能修这个
问题：它们是 content-level 或 end-of-stream 修复，不改流形状。
`ResponsesStreamHealer` 隐式假设「单 message item」，碎片化流破坏这个
假设（`healed_text` 只累积首碎片的 delta，后续 13 个碎片被忽略）。

## Goal

透传路径上加一道 **流形状修复**：当上游把一个逻辑 item 拆成 N 个连续
`output_item.added` 时，daemon 把它们合并成一个 item，所有 deltas 重写到首
碎片的 `item_id` / `output_index`，run 末尾 flush 合成 done。**健康流
（run length 永远 = 1）零开销**。

合并范围：同 type 的连续 item（`message` / `reasoning` / 同 `call_id` 的
`function_call`）。跨类型、跨 call_id、跨非以上类型不合并。

## Mechanism (verified against MiniMax M3)

NOTES §2 已逐变量排除：

- **与 headers 无关**（UA / originator / OpenAI-Beta / Accept 全加上照碎）
- **与 reasoning effort 无关**（high / 默认都试过）
- **与输出长度无关**（短句 / 150 词长文）
- **与历史回放无关**（纯文本 / 含 reasoning items）
- **与 codexferry 无关**：透传转发链（0.0.0-main 与 release 0.1.2）实测
  都正确转发 `instructions` / `tools`；真实 codex 请求体（trace 捕获，9
  个 tools）回放到两个版本上均合规

最可能的触发条件是上游网关序列化器按请求体形态切换；与 daemon 行为无关。

## Design

### Overview

```
上游 chunk → split_sse_events(stream) → FragmentedItemMerger.push_event
                                          ↓ (改写后的 bytes)
                                       ResponsesStreamHealer.push_event
                                          ↓
                                       tx.send(Ok(bytes)) → client
                                          ↑
                                     现有路径（已存在）
```

- 新模块 `src/heal/merge.rs`（与 `dsml.rs` / `think.rs` / `responses.rs` 同级）
- 接入：`src/proxy/passthrough.rs` 的 healed 分支，**在 `ResponsesStreamHealer` 前面**
- chat 路径完全不动（`StreamConverter` 是构造者姿态，`message_output_index`
  整 turn 至多 None→Some 一次的不变量是 chat 天然不碎片的物理保证）
- e2e / 集成 / 单测三层覆盖（见 Testing）

### Interface

```rust
// src/heal/mod.rs — 新增第三字段
pub struct HealGates {
    pub dsml: bool,             // 已存在
    pub think: bool,            // 已存在
    pub merge_fragmented: bool, // 新增，默认 true
}
impl Default for HealGates {
    fn default() -> Self { HealGates { dsml: true, think: true, merge_fragmented: true } }
}

// src/quirks.rs — 注册名
pub const QUIRK_NAMES: &[&str] = &[
    "glm_thinking", "missing_done", "dsml_heal", "think_tags", "merge_fragmented",
];

// src/heal/merge.rs — 新文件
pub struct FragmentedItemMerger {
    enabled: bool,
    run: Option<RunState>,
}
struct RunState {
    item_type: ItemType,        // Message | Reasoning | FunctionCall
    start_idx: usize,           // 首碎片 output_index
    start_id: String,           // 首碎片 item.id（保持透传给 client）
    call_id: Option<String>,    // 仅 FunctionCall 时 Some
    merged_text: String,        // message 累积
    merged_reasoning: String,   // reasoning 累积
    merged_arguments: String,   // function_call 累积
    part_added_emitted: bool,   // 首碎片 content_part.added 是否已发
}
enum ItemType { Message, Reasoning, FunctionCall }

impl FragmentedItemMerger {
    pub fn new(enabled: bool) -> Self;
    /// 与 ResponsesStreamHealer::push_event 完全对称的签名
    pub fn push_event(&mut self, raw: &[u8], event: Option<&str>, data: &str) -> Vec<Bytes>;
    pub fn finish(&mut self) -> Vec<Bytes>;
}
```

API 签名故意与 `ResponsesStreamHealer` 对称，让 passthrough 的连接代码
是「再链一个 filter」：

```rust
// src/proxy/passthrough.rs 的 healed 分支（伪代码）
let mut merger = FragmentedItemMerger::new(heal.merge_fragmented);
let mut healer = ResponsesStreamHealer::new(heal);
'loop: for evt in events {
    for chunk in merger.push_event(&evt.raw, evt.event.as_deref(), &evt.data) { tx.send(...); }
    for chunk in healer.push_event(&evt.raw, evt.event.as_deref(), &evt.data) { tx.send(...); }
}
for chunk in merger.finish() { tx.send(...); }   // γ-1 通常无 op
for chunk in healer.finish() { tx.send(...); }
```

### State machine

状态名澄清：`无 run` → `跟踪中`（已透传第 1 个 added，run.length = 1，
尚未看到第 2 个） → `合并中`（看到第 2 个同 type added，run.length ≥ 2，
抑制后续 added）。区分这两个状态是因为 length=1 时 run 没真正合并，
合成 done 没意义。

| 当前状态 | 事件 | 动作 |
|---|---|---|
| 无 run | `output_item.added` (any type) | 透传原样；**不**开始 run（要等下一个 added 才能判断 run length） |
| 无 run | `output_text.delta` / `content_part.added` / `*.done` / 其他 | 透传原样 |
| 无 run | `response.completed` | 透传原样 |
| **跟踪中** (run.length = 1) | 第 2 个**同 type + 同 call_id** 的 `output_item.added` | **进入合并**：抑制本 added；累积 merged_*；后续 content_part.added / done 也抑制 |
| 跟踪中 | 第 2 个**不同 type 或不同 call_id** 的 `output_item.added` | 丢弃跟踪（length=1 无 done 待 flush）；开始新跟踪，透传本 added |
| 跟踪中 | `response.completed` | 透传原样（length=1 没合成 done 的必要）|
| **合并中** (run.length ≥ 2) | 同 type + 同 call_id 的 `output_item.added` | 抑制；累积 |
| 合并中 | 不同 type 或不同 call_id 的 `output_item.added` | **flush 当前 run**（合成 content_part.done + output_item.done）；开始新跟踪，透传本 added |
| 合并中 | `content_part.added` | 首碎片已发（无 run 阶段或合并中首事件时已透传）则抑制；否则透传并标记 `part_added_emitted = true` |
| 合并中 | `output_text.delta` (message) / `response.reasoning_summary_text.delta` (reasoning) / `response.function_call_arguments.delta` (function_call) | **重写 `item_id` / `output_index`** 为首碎片的；文本字段原样透传；累积 merged_* |
| 合并中 | `output_text.done` / `content_part.done` / `output_item.done` | 抑制（run 末尾会 flush 合成 done） |
| 合并中 | `response.completed` | 先 flush 当前 run 的合成 done，再透传 `response.completed` |
| `finish()` (stream 结束) | — | **不合成 done**（γ-1：截断就截断；passthrough.rs 已经发 `response.failed` 收尾） |

### Event rewriting rules（合成 done 的精确格式）

`content_part.done`（合成）：

```
event: response.content_part.done\n
data: {"type":"response.content_part.done","item_id":"<start_id>","output_index":<start_idx>,
       "part":{"type":"output_text","text":"<merged_text>"}}\n\n
```

`output_item.done`（合成）：

```
event: response.output_item.done\n
data: {"type":"response.output_item.done","output_index":<start_idx>,
       "item":{"type":"message","id":"<start_id>","role":"assistant","status":"completed",
               "content":[{"type":"output_text","text":"<merged_text>"}]}}\n\n
```

（reasoning / function_call 形态见 `sse_block(event, payload)` 模板，
字段名与 OpenAI Responses 一致；reasoning 用
`response.output_item.done` + item.type="reasoning" + summary；
function_call 用 item.type="function_call" + call_id / name / arguments）

格式参考 `src/heal/responses.rs::sse_block`（已有的 SSE 块构造器）。

### Interaction with ResponsesStreamHealer（D-1：完全正交）

- merger 只管 item 形状合并；healer 只管内容修复
- merger 改写后传给 healer 的事件是「真的只有一个 message item」 —— **消除了
  review #5/#7 的 multi-item 边界**（NOTES §6.3 已点出）
- 合并 run 里嵌 DSML markup：merger 改 item_id → healer 过 DSML filter → 修复
- 合并 run 里嵌 think markup：merger 改 → healer 过 think filter → 分流 reasoning
- 合并 run 里 DSML + think 双泄漏：两阶段管道照常

### Session capture interaction

`last_completed_payload`（`src/proxy/capture.rs`）从转发字节读
`response.completed` 的 output 数组。merger 改写后该数组只有合并后的 items；
`completed_capture(output)` 自然存合并版本。session replay 跨路径一致。

### Kill switch & defaults

- `merge_fragmented: bool` 默认 `true`（与 `dsml_heal` / `think_tags` 一致，
  deny-by-default 哲学）
- `[quirks] disabled = ["merge_fragmented"]` 关闭
- 热重载：与现有 quirk 一致，watcher → channel → applier，**下一个请求**
  拿到新 gate（AGENTS.md §7）
- `config.quirk_enabled("merge_fragmented")` 在 `passthrough.rs` 与
  `HealGates::new` 之间一次性读

## Testing

### Unit fixtures（`src/heal/merge_tests.rs`，新文件）

**触发/不触发边界（9 case）**：

| ID | 场景 | 期望 |
|---|---|---|
| M1 | 1 个 message item（合规）| 完全透传，run length=1 不合并 |
| M2 | message × N 连续（碎片）| 合并成 1 个 item，N-1 个 added 抑制 |
| M3 | 1 个 reasoning item（合规）| 透传 |
| M4 | reasoning × N 连续 | 合并 |
| M5 | 1 个 function_call（合规）| 透传 |
| M6 | function_call × N **同 call_id** | 合并 |
| M7 | function_call × N **不同 call_id** | **不合并**，独立 item |
| M8 | message + reasoning + function_call type 切换 | 按 type 切，每段独立 |
| M9 | message × N + reasoning + message × M 交错 run | 每段独立合并，reasoning 不动 |

**改写正确性（5 case）**：

| ID | 场景 | 期望 |
|---|---|---|
| W1 | 后续 fragment 的 `item_id` / `output_index` 重写 | 一致 |
| W2 | 后续 fragment 的 `content_part.added` 抑制 | 不出现 |
| W3 | 后续 fragment 的 `output_item.done` / `content_part.done` 抑制 | 不出现 |
| W4 | run 末尾补发的 `content_part.done` 文本 = 合并累积全文 | 顺序正确 |
| W5 | run 末尾补发的 `output_item.done` 的 `item.content[0].text` = 合并累积 | 同上 |

**边界与失败（4 case）**：

| ID | 场景 | 期望 |
|---|---|---|
| E1 | 上游 run × 14（NOTES worst-case）| 合并无 bug |
| E2 | run 中切到 function_call | message run 在切换前 flush done |
| E3 | 上游只发首碎片 added + 几个 delta 就断流 | γ-1 截断就截断；finish() 不合成 done |
| E4 | 上游发的 fragment item.id 是空串 | 透传（容忍）|

**与 ResponsesStreamHealer 串联（3 case）**：

| ID | 场景 | 期望 |
|---|---|---|
| S1 | 合并 run 嵌 DSML markup | merger 改 item_id → healer 修 DSML |
| S2 | 合并 run 嵌 think markup | merger 改 → healer 分流 reasoning |
| S3 | 合并 run 嵌 DSML + think 双泄漏 | 两阶段管道照常 |

**杀开关（1 case）**：

| ID | 场景 | 期望 |
|---|---|---|
| K1 | `merge_fragmented` disabled | merger 是 identity，所有事件透传 |

**总计 22 fixture**（介于早期估算的 15-20 与实际需要之间；M1/M3/M5/M7/K1
是反向 fixture，确保 trigger 不误判）。

### Integration tests（`tests/passthrough.rs` 追加）

追加 2-3 case，共享 `tests/common/mod.rs` 的 harness：

1. mock 上游发 fragmented SSE 流（`MockState` 加新字段或新 handler，
   沿用 `responses_frag` 命名），客户端断言 `output_item.added` 事件数 ≤ N（合并后）
2. mixed 场景：merged run + 独立 reasoning item + fragmented function_call
   同 call_id，端到端断言 item 数与 item 内容

### E2E（暂缓）

NOTES §6.3 明确写「**启动时机：再次复现时**」。等真复现时再补：

- `src/bin/e2e-mock.rs` 加 `responses_frag` scenario
- `scripts/e2e.sh` 加 `fragmented` case
- `src/heal-description.md` §7 模块目录表行数更新

## Decisions

recap brainstorming Q1–Q5：

| | 决策 | 备注 |
|---|---|---|
| **Q1** | 独立 `heal/merge.rs` pass，仅 responses；chat 路径完全不动 | 排除 (B) 折进 ResponsesStreamHealer（违反 per-quirk 模块切分）、(C) 通用流水线（over-engineering）|
| **Q2** | 类型 = message + reasoning + function_call 同 call_id；交错 = 紧邻 run | 选项 2 + 方案 X；覆盖 OpenAI 契约「同 call 必须同 item」条款 |
| **Q3** | 默认 ON；trigger = run length ≥ 2；截断 = γ-1（不合成 done）| 与 dsml/think 一致的 deny-by-default 哲学 |
| **Q4** | A-1（不动文本）+ B-1（抑制所有 done，run 末尾 flush）+ C-1（type 切换前 flush）+ D-1（merger 只管形状）| NOTES §6.3 写法 |
| **Q5** | 单测 + 集成 + e2e 暂缓 | 22 fixture + 2-3 集成 case |

## Out of scope

- e2e 脚本补全（NOTES §6.3 写再次复现时）
- AGENTS.md 加 `merge_fragmented` 行（不强制，与现有 quirk 一致）
- StreamConverter 加显式不变量文档（顺手做的小改进，独立 commit；spec 落地后可单独提）
- 多 item 类型合并（file_search_tool_call / web_search_tool_call /
  computer_call / code_interpreter_call …，未观测，YAGNI）
- 跨类型交错时持续合并（方案 Y，未观测）
- 上游 broken 上更激进的修复（比如把 fragmented tool_calls 拼成完整
  arguments 字符串后只发一个 item —— 实际上是 §11 S1 的 fixture 已验证）

## Risks & mitigations

| 风险 | 缓解 |
|---|---|
| **R1：合规流被误合并** | trigger 严格（同 type + 同 call_id for function_call），单测 M1/M3/M5/M7/K1 是反向 fixture |
| **R2：合成 done 格式错** | 严格按 Responses wire 格式（参考 `sse_block` 模板）；W4/W5 fixture 校验 |
| **R3：与 ResponsesStreamHealer 顺序敏感** | 测试矩阵 S1–S3 验证串联；单元独立、集成端到端 |
| **R4：上游行为变化** | self-deactivate（健康流 run length=1 永远不触发）；杀开关 disable |
| **R5：M3 修好后自动失活** | 触发只在「裸聊天形态」出现；真 codex 请求走合规路径，merger 永不触发（NOTES §6.3 已点）|
| **R6：聚合 run 长度无界** | memory 边界由 SessionStore 的 TTL + LRU + `max_memory_mb` 把控（AGENTS.md §6）；merger 不持有额外状态超出单 run 生命周期 |

## Docs sync (repo convention §13)

实施后**立即**更新 `docs/heal-description.md`（不是写 spec 时改）：

- §3 总览表加 `merge_fragmented` 行（方向 = response，子类 = 流形状修复，
  触发对象 = 同 type + 同 call_id 连续 `output_item.added`，默认 ON，进
  `HealGates` 是）
- §6 加 6.5 节：方向 / 子类 / 适用路径 / 触发条件 / 修复做法 / Hook 点 /
  `HealGates::merge_fragmented` 字段引用 / 默认
- §7 模块目录加 `merge.rs` 行（行数待实施后填）
- §8 state machine 对照表加 FragmentedItemMerger 列（结构同上）
- §12 总结从「stream-shape heal 仅 responses」改为「stream-shape heal 在
  responses 上由 merge_fragmented 提供」

不更新 AGENTS.md（现有 quirk 加字段时也未改 AGENTS.md，保持现状）。如未来
需要在 AGENTS.md 里列出 `HealGates` 字段，由实施者根据当时内容布局自行决定插入
位置（§4 session keying / §8 SSE events / §8b item-merge loop 都是候选）。

不更新 ARCHITECTURE.md（架构未变化，新加的只是 `heal/` 的子模块）。

## Verification (post-implementation matrix)

实施完成后按下列顺序验证：

1. `cargo build --release` —— 无 warning
2. `cargo test` —— 所有单测 + 集成测试通过
3. `cargo test heal::merge_tests` —— 22 fixture 全过
4. `cargo test passthrough` —— 集成测试通过
5. `RUST_LOG=codexferry=debug` 跑 `cxf.toml` 配 mock upstream 的 daemon，
   故意触发一次 fragmented 流（用 NOTES §4.2 一行计数器判碎）—— 确认
   客户端看到的 `output_item.added` 数 ≤ 2（合规 message + 合并 run）
6. 健康流无 `quirk merge_fragmented fired` warn（self-deactivate）
7. `codex -m minimax/MiniMax-M3 -c 'model_provider=...'` 真请求一次：
   - TUI 不再切成多个 bullet
   - `codexferry` 日志里 `passthrough stream completed` 状态 200
8. `scripts/release.sh vX.Y.Z --prep-changelog` 出 CHANGELOG 条目
