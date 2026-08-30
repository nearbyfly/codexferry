# MiniMax-M3 输出碎片化调查记录（未复现，挂起观察）

> **本地笔记，不进 git**（已加入 `.git/info/exclude`）。
> 日期：2026-08-28 ｜ 环境：codexferry 0.0.0-main（main@0d0dae2）+ codex-cli 0.147.0
> ｜ 上游：`https://api.minimaxi.com/v1`，路由 `minimax/MiniMax-M3`（format="responses" 透传）
> 状态：**症状真实发生过一次，当前全栈无法复现；上游缺陷仍活着（裸请求必碎），
> 触发入口暂时不被 codex 流量命中。等再次出现按本文手册处理。**

## 1. 现象

Codex TUI 中一条本应连续的助手回复被拆成多个 `•` bullet，断点在句子中间
（chunk 边界）：

```
• Hey there
• ! How's it going? What are we diving
• into today?
```

wire 层表现：MiniMax M3 把**每个流式 chunk 发成一个独立 output item**
（每个碎片一组完整的 `output_item.added → content_part.added → 1个delta →
done → output_item.done` 生命周期），Codex 逐 item 渲染
（`codex-rs/core/src/stream_events_utils.rs:288` 的 `handle_output_item_done`），
所以一个 item 一个 bullet。`response.completed` 的 output 数组同样带全部碎片
（历史回放也会碎）。同网关的 M2.7、以及 GLM/DeepSeek 全程合规（1 reasoning +
1 message，文本在 item 内用 delta 流式）。

## 2. 已确认的机理（可随时复现）

**M3 的 Responses 网关有两套流式序列化器，按请求体形态切换：**

- **裸聊天形态**（无 `instructions` 且无 `tools`）→ 坏序列化器：按 chunk 拆
  item。**今天仍然每次必碎**（复现见 §4，典型 5–14 items，数量随 chunk 划分
  随机波动）。reasoning 碎片共用同一 id（`..._rs`），message 碎片带递增后缀
  （`..._msg_9/10/11`）。
- **带 `instructions`（或 `tools`）的 agentic 形态**（= 所有真实 codex 请求）→
  合规序列化器：单 item + delta 流式。稳定 2 items（1 reasoning + 1 message）。

**与以下变量无关（逐一实验排除）**：headers（codex UA / originator /
OpenAI-Beta / Accept 全加上照碎）、reasoning effort（high/默认都试过）、
输出长度（短句/150 词长文）、历史回放（纯文本 / 含 reasoning items）。

**codexferry 无责**：透传转发链两代二进制（0.0.0-main 与 release 0.1.2）实测
都正确转发 `instructions`/`tools`；真实 codex 请求体（trace 捕获，9 个 tools）
回放到两个版本上均合规。

## 3. 未解之谜

用户原始碎片会话的**具体触发条件不可追溯**（当时无 wire 抓包）。已排除：
新旧二进制、tools/instructions 缺失、延续会话/切模型的历史回放（含 reasoning
items）、headers、effort。最可能：MiniMax 侧在间隔窗口内行为变化（M3 网关活跃
迭代，坏序列化器还在但入口变窄）；次可能：当时瞬态（灰度/负载路径）。
用户回忆碎片会话发生在**中途切换模型**（"Model changed to minimax/MiniMax-M3
high"）之后 —— 此关联未被证实也未被证伪。

## 4. 复现 / 判别手册

### 4.1 一行计数器（判碎利器）

```bash
count_items() { python3 -c "
import sys
n=0
for line in sys.stdin:
    if line.startswith('data: ') and 'response.output_item.added' in line: n+=1
print(n)
"; }
# 用法：curl -sN ... | count_items   →  2=合规；≥4=碎片化
```

### 4.2 裸形态（应碎 —— 验证坏序列化器还在）

```bash
KEY=$MINIMAXI_API_KEY
curl -sN --max-time 60 https://api.minimaxi.com/v1/responses \
  -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' -d '{
  "model":"MiniMax-M3",
  "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Reply with one short greeting sentence"}]}],
  "reasoning":{"effort":"high"},"stream":true,"store":false}' | count_items
```

### 4.3 agentic 形态（应干净 —— 验证 codex 流量路径健康）

同上，body 加 `"instructions":"You are a helpful assistant."` 即可
（tools 可省；instructions 单独存在就切合规路径）。

### 4.4 真实 codex 形态回放

用 §5 抓到的 body 文件直接 `-d @/tmp/real-codex-body.json` 打 daemon。

## 5. 下次出现时的抓证据流程（两分钟）

```bash
# 1. 起 traced 第二实例（不打扰常驻 daemon；端口随意换）
sed 's/^port = 8787/port = 8791/' ~/bin/cxf.toml > /tmp/cxf-trace.toml
CODEXFERRY_CONFIG=/tmp/cxf-trace.toml CODEXFERRY_TRACE_BODY=1 \
  RUST_LOG=codexferry=debug nohup ~/bin/codexferry > /tmp/cxf-trace.log 2>&1 &

# 2. codex 指到它复现（scratch home，不动真实 ~/.codex）
mkdir -p /tmp/codex-dbg && cd /tmp/codex-dbg
codex -s read-only --skip-git-repo-check \
  -c 'model_provider="cxfdbg"' \
  -c 'model_providers.cxfdbg.name="cxfdbg"' \
  -c 'model_providers.cxfdbg.base_url="http://127.0.0.1:8791/v1"' \
  -c 'model_providers.cxfdbg.wire_api="responses"' \
  -c 'model_providers.cxfdbg.env_key="MINIMAXI_API_KEY"' \
  -m minimax/MiniMax-M3

# 3. 从 /tmp/cxf-trace.log 提取 "request body:" 行（= codex 发来的完整请求体），
#    用 4.4 回放计数；对照出站转发体是否丢了 instructions/tools。
```

关键判读：log 里 `request body:` 是入站体；若入站体带 instructions/tools 而上游
仍碎 → MiniMax 侧回归；若入站体缺字段 → codexferry/catalog 侧问题。

## 6. 应对层级（由快到慢）

1. **抓证据定位**（§5，两分钟）。
2. **零代码规避**：给 M3 路由复制一个 chat-format provider（`format` 挂在
   provider 上，直接改 `[providers.minimax]` 会连累 M2.7）：
   ```toml
   [providers.minimax-chat]
   base_url = "https://api.minimaxi.com/v1"
   api_key_env = "MINIMAXI_API_KEY"
   format = "chat"
   # [routes] 里把 "minimax/MiniMax-M3" 的键名改成 "minimax-chat/MiniMax-M3"
   ```
   chat 转换路径的 SSE itemization 由 codexferry StreamConverter 生成，必然规范。
3. **healing 修复**（防御性加固，设计已 scope 好、未写 spec/plan）：
   在 `heal/responses.rs` 的 `ResponsesStreamHealer` 前面加一级合并状态机
   （新文件 `heal/merge.rs`，方案 A）：连续同类型（message/reasoning）item 游程
   合并 —— 首碎片的 added/part.added 与所有 delta 照发、后续碎片的 added 与各
   碎片的 done 全吞、游程结束时补发完整 done 序列；`response.completed` 的
   output 数组同样合并（session capture 解析的是转发后的 completed，一处修复
   同时治渲染与历史回放）。结构触发（游程 ≥2 才动手），规范流零改动，配
   `[quirks] disabled` kill switch，默认开（与 dsml_heal/think_tags 同族）。
   验证深度已选定：fixture 单测 + e2e-mock 碎片场景。启动时机：再次复现时。

## 7. 下次发版备忘（与本文主题无关，仅借本地记录防遗忘）

- **README 瘦身**：README 的 "### Hiding Codex's bundled GPT models" 小节太详细
  （TOML 示例 + 降级行为说明，~15 行）。下次发版时把详细配置说明挪到新的
  `README-DETAILS.md`，README 只留一句话 + 链接。
- release.sh 的 `--force-with-lease` 参数顺序 bug 已在 v0.1.3 发布后修复
  （`1fe3ef6`），下次发版应可一次跑通。

## 8. 相关事实存档

- 非流式 M3 响应合规（2 完整 items）—— 只有 streaming 序列化器有问题。
- M2.7 的 message 文本带前导 `\n\n`（另一个无害小怪癖，单 bullet 渲染不受影响）。
- codexferry 常驻 daemon：`~/bin/codexferry` → 符号链接 `codexferry-0.0.0`，
  配置 `~/bin/cxf.toml` → 符号链接本仓库 `cxf.toml`；`MINIMAXI_API_KEY` 在
  daemon 进程环境里（可从 `/proc/$(pgrep codexferry)/environ` 取）。
- 调查期间确认：`send_upstream` 只发 Authorization + Content-Type + extra_headers
  （不转发 codex 的 UA/originator/OpenAI-Beta）—— 已实验证明这些头与碎片化无关。
