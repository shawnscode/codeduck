# codeduck

基于 **Difftastic (`difft`)**、**Rust** 与 **Vim 操作模式** 的终端 Code Review 工具。
优先完整支持 **SVN**（同时内置可扩展的 `VcsBackend` 抽象并已支持 **Git**），可直接调用用户本地已登录订阅的 AI CLI（如 `claude`、`codex`、`gemini` 或自定义 CLI），对**未提交改动**或**指定 commit/revision** 进行只读 AI 分析、语义分类与分组，辅助高效 review。

## 前置依赖

- **Difftastic**: `difft` 已安装并在 `$PATH` 中（例如 `brew install difftastic`）
- **SVN** (`svn`) 或 **Git** (`git`)
- 可选本地已登录的 AI CLI（无需额外配置 API Key，支持跨重启复用本地对话 session 并记录到 `.codeduck` 的 `[ai.sessions]`）：
  - `agy` (Google Antigravity CLI，支持 `--model`、`--effort`、`--resume`)
  - `codex` (OpenAI Codex CLI，只读沙箱执行并通过 `exec resume` 复用会话)
  - `claude` (Claude Code CLI，使用 `--tools ""` 与 `--resume` 复用只读会话)
  - `gemini` (独立 Gemini CLI，使用 `--approval-mode plan` 与 `--resume` 复用会话)

## 编译与运行

```bash
cargo build --release

# 1. 在 SVN 工作副本中 review 当前未提交改动（自动检测本地可用 AI CLI）
cargo run --

# 2. Review 指定 SVN revision（例如 r123）或范围（r100:r120）
cargo run -- -r 123
cargo run -- --range 100:120

# 3. 指定本地 AI 订阅工具、模型、思考强度，或关闭/重置对话 session
cargo run -- --ai agy --model gemini-4-hydrogen-high --effort medium
cargo run -- --ai agy --new-session
cargo run -- --ai codex --effort low
cargo run -- --ai claude
cargo run -- --ai none

# 4. 非交互导出 Markdown Review 报告
cargo run -- -r 123 --headless-export review-r123.md
```

## 交互逻辑与 Vim 快捷键

采用**双栏交互 + 底部被动信息联动面板**设计（底部信息面板无需手动聚焦或选中操作）：
- **聚焦左侧重点树时**：树节点按 AI 语义分类展示每个重点的**简易标题 + 简短描述**，上下移动 (`j`/`k`) 时，右侧自动定位到对应代码，底部面板自动显示该分类的完整说明与核查要点。
- **聚焦右侧连续 Diff 时**：右侧展示连续的跨文件 Diff 流，使用 `h`/`j`/`k`/`l` 移动光标与左右滚屏，`n`/`N` 跨文件跳转到下一个/上一个差异处，底部面板自动联动展示当前光标所在段落的 AI 分析与问题发现。

| 按键 | 功能 |
| --- | --- |
| `Tab` / `Ctrl-w` / `H` / `L` | 在「左侧重点树」与「右侧连续 Diff」间切换焦点 |
| `j` / `k` | 向下 / 向上移动（左侧切重点/文件，右侧移动光标跨文件滚动） |
| `h` / `l` | 左侧折叠 / 展开分组；右侧左右滚动 Diff |
| `n` / `N` | 跨文件跳转到下一个 / 上一个差异处（`]c` / `[c` 亦可） |
| `]f` / `[f` | 切换下一个 / 上一个文件 |
| `]g` / `[g` | 跳到下一个 / 上一个 AI 重点分组 |
| `]i` / `[i` | 跳到下一个 / 上一个 AI 问题发现 |
| `Ctrl-d` / `Ctrl-u` / `Ctrl-f` / `Ctrl-b` | 半页 / 整页上下滚动 |
| `gg` / `G` | 跳到顶部 / 底部 |
| `d` | 切换 Difftastic 视图：`side-by-side` / `inline` / `side-by-side-show-both` |
| `t` | 在「AI 重点分组视图」与「平铺文件列表」间切换 |
| `za` / `zM` / `zR` | 折叠/展开当前组 / 全部折叠 / 全部展开 |
| `S` | 打开提交面板，按分类组勾选文件并自动生成英文提交日志，进入二次提交确认弹窗（`:ci` / `:submit` 亦可） |
| `/` 与 `;` / `,` | 在连续 Diff 中搜索并跳转下一 / 上一匹配 |
| `c` | 为当前文件添加个人 Review 批注 |
| `A` | 针对当前文件 Diff 向本地 AI 追问 |
| `a` / `P` | 重新触发 AI 分析 / 切换本地 AI CLI 工具 |
| `C` | 打开 Commit / Revision 选择器（可切回 Uncommitted） |
| `R` | 强制刷新当前改动、Diff 与提交历史缓存 |
| `e` | 导出 Markdown Review 报告 (`codeduck-review.md`) |
| `:` | 进入 Vim 命令行模式（`:rev 123`、`:uncommitted`、`:ignore *.meta`、`:ai codex`、`:new-session`、`:ask 问题`、`:export out.md`、`:q`） |
| `?` | 打开完整帮助弹窗 |

## 配置文件（项目 `.codeduck` 与通用 `~/.codeduck/configs.toml`）

通用配置、会话记录与缓存统一存放于 **`~/.codeduck/`**（支持通过环境变量 `CODEDUCK_HOME` 覆盖）：
- 通用配置：`~/.codeduck/configs.toml`（与项目 `.codeduck` 采用完全一致的 `[ai]` / `[diff]` / `[ignore]` / `[[custom_ai]]` TOML 格式，也兼容扁平键）
- 对话会话：`~/.codeduck/sessions.toml`（按工作区路径隔离记录各项目的 AI 对话 session ID，不再写回或污染项目 `.codeduck`；旧版项目 `.codeduck` 中的 `[ai.sessions]` 仍兼容读取）
- 本地缓存：`~/.codeduck/cache/`（AI 审阅缓存、`history/` 历史列表缓存、`history_diffs/` 不可变历史 Diff 缓存）

配置优先级为：`默认配置 < ~/.codeduck/configs.toml < 项目 .codeduck (沿目录层级) < 命令行参数`；当前目录下找不到的配置项或全局忽略规则，会自动从通用配置 `configs.toml` 中读取。

项目 `.codeduck` 示例：

```toml
[ai]
provider = "agy"                    # auto | agy | codex | claude | gemini | none | 自定义名称
model = "gemini-4-hydrogen-medium"  # 主模型：用于风险发现与追问（可通过 `agy models` 查看）
effort = "medium"                   # 主模型思考强度 (low | medium | high | xhigh | max)
fast_model = "gemini-4-hydrogen-low" # 快速模型：用于快速分组与生成英文提交说明
fast_effort = "low"                 # 快速模型思考强度
language = "zh-CN"
timeout_secs = 90
reuse_session = true                # 深度审查与追问复用会话；分组与提交说明默认无状态快速运行

[diff]
display_mode = "side-by-side"       # side-by-side | inline | side-by-side-show-both
ignore_comments = false
cache_history = true                # 缓存历史提交列表与不可变 revision diff（HEAD 无变动时跳过 svn log -v）

[ignore]
# 兼容 .gitignore 语法的忽略规则（也可在 TUI 中通过 `:ignore <规则>` 追加）
patterns = [
  "*.meta",
  "!keep.meta",
  "3rds/",
]
```

通用 `~/.codeduck/configs.toml` 示例（支持与项目 `.codeduck` 相同的分组语法，当前目录缺省项自动回退到这里）：

```toml
[ai]
provider = "auto"        # auto | agy | codex | claude | gemini | none | 自定义名称
# model = "gemini-4-hydrogen-medium"
# effort = "medium"
language = "zh-CN"
timeout_secs = 90
reuse_session = true

[diff]
display_mode = "side-by-side"
ignore_comments = false
cache_history = true

[ignore]
# 全局兜底忽略规则（项目 .codeduck 中可用 !规则 局部取消忽略）
patterns = []

# 可选：接入其他本地 AI CLI（从 stdin 读取 prompt，stdout 返回结果）
# [[custom_ai]]
# name = "my-ai"
# command = "my-ai-cli"
# args = ["--print"]
```
