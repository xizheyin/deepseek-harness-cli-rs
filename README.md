<div align="center">
  <h1><code>dsh-rs</code></h1>
  <p><strong>用 Rust 构建的 DeepSeek 终端编程 Agent</strong></p>
  <p>在真实代码仓库里持续对话：搜索和阅读代码、应用补丁、运行命令，并在长会话中保存、恢复与压缩上下文。</p>
  <p>
    <a href="https://github.com/xizheyin/deepseek-harness-rs/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/xizheyin/deepseek-harness-rs/actions/workflows/ci.yml/badge.svg"></a>
    <a href="Cargo.toml"><img alt="Version 0.1.0-alpha.0" src="https://img.shields.io/badge/version-0.1.0--alpha.0-f59e0b"></a>
    <a href="rust-toolchain.toml"><img alt="Rust 1.85.0" src="https://img.shields.io/badge/Rust-1.85.0-000000?logo=rust"></a>
    <a href="LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-2563eb"></a>
  </p>
  <p>
    <a href="#快速开始">快速开始</a> ·
    <a href="#能力概览">能力概览</a> ·
    <a href="#会话与长对话">会话恢复</a> ·
    <a href="#安全边界">安全边界</a> ·
    <a href="#项目状态">项目状态</a>
  </p>
</div>

> [!WARNING]
> `dsh` 当前是 `0.1.0-alpha.0` 预发布版本，源码安装候选已通过 Phase 0–9
> 验收，但尚无受支持的稳定发行版、预编译包或 crates.io 发布。

`dsh-rs` 是项目名，安装后的命令是 `dsh`。这是一个独立的社区开源项目，Agent 内核以固定版本的
[DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) 为行为参考，
再用适合 Rust CLI 的类型、并发模型和安全边界重新实现；它不是官方产品，也不是
TypeScript 源码的逐行翻译。

<p align="center">
  <img src="docs/assets/dsh-overview.png" alt="dsh-rs 在终端中读取文件、展示补丁并完成修改" width="1128">
</p>

<p align="center"><sub>Phase 9 旧版滚动输出界面的真实安装证据；模型响应来自离线 loopback fixture，不调用真实 API，也不消耗额度。Phase 11 增强界面的新截图仍待最终验收生成。</sub></p>

## 快速开始

仓库固定使用 Rust 1.85.0。安装 [Rustup](https://rustup.rs/) 后，在仓库根目录执行：

```console
cargo install --locked --path .
export DEEPSEEK_API_KEY='你的 DeepSeek API Key'
dsh --workspace .
```

默认 `--tui auto` 会在有颜色、`xterm*`、非 tmux/Screen/Zellij 且初始窗口至少
44×12 时启用增强界面。看到 `❯` 后直接输入任务并按回车；其他环境会保守使用
零 ESC 的 `dsh >` 线性界面。可用 `--tui enhanced` 或 `--tui linear` 明确选择；
`--no-color`、`NO_COLOR` 和 `TERM=dumb` 会强制线性界面。例如：

```text
请先了解这个项目，再告诉我最值得修复的三个问题。
```

API Key 只从进程环境按请求读取。不要把真实密钥写入提示词、工具参数或 Shell
命令，因为这些内容本来就是模型和会话可见的。

## 能力概览

| 能力 | 当前实现 |
| --- | --- |
| 多步骤 Agent Loop | 流式接收 DeepSeek 响应，关联 reasoning、文本、工具调用、结果、usage 和结束原因 |
| 代码理解 | 工作区内的 `list`、`glob`、`grep` 和 `read`，输出和扫描范围均有上限 |
| 文件修改 | 严格的单文件 `apply_patch`，执行前展示实际 diff，并检查路径、符号链接和并发修改 |
| 命令执行 | 经审批的前台 `bash`，限制输出和运行时间，并在正常可观察路径下终止、回收同进程组工作 |
| 交互控制 | 实验性 Unicode 多行 Composer、忙时下一回合队列、动态 Dock、安全粘贴、审批、Ctrl+C 取消，以及严格线性后备 |
| 脚本模式 | `--prompt` 或管道输入；不会停下来等待审批，并安全拒绝写文件或 Shell 请求 |
| 长会话 | 有上限的本地 JSONL、会话列表与恢复，以及一次有界的自动上下文摘要 |
| 本地工具插件（实验性） | 显式配置受信任的子进程工具；协议、队列、输出、超时和清理都有上限，交互调用仍需审批 |

## 使用方式

### 交互模式

```console
dsh --workspace .
```

增强界面提供下面的 Composer 编辑键；若自动回退到线性兼容界面，则按终端的普通
整行输入方式操作。

| 输入 | 行为 |
| --- | --- |
| `/help` | 显示会话内帮助 |
| `/exit` 或 `/quit` | 等待清理后退出 |
| <kbd>Enter</kbd> | 空闲时发送；当前回合运行时加入下一回合 FIFO |
| <kbd>Ctrl</kbd> + <kbd>J</kbd> | 在增强 Composer 中插入换行 |
| 方向键、Home/End、Backspace/Delete | 按 Unicode 字素编辑；上下方向在边界浏览本进程已提交历史 |
| <kbd>Ctrl</kbd> + <kbd>R</kbd> / <kbd>Ctrl</kbd> + <kbd>_</kbd> | 反向搜索历史 / 撤销；Ctrl+Z 保留给暂停 |
| <kbd>Ctrl</kbd> + <kbd>C</kbd> | 取消当前回合，清理完成后继续当前会话 |
| <kbd>Ctrl</kbd> + <kbd>D</kbd> | 安全结束会话 |
| <kbd>Ctrl</kbd> + <kbd>Z</kbd> | 先清理当前回合再暂停；回到 Shell 后可用 `fg` 恢复 |

文件修改、Shell 或插件执行前会显示完整预览和三项选择器。增强界面默认选中
**Reject**；只有当前审批中先按方向键、再单独按一次 <kbd>Enter</kbd> 才能授权。
同一次读取里的“方向键+回车”、可打印的 `y`、粘贴、Ctrl+J 和未知转义序列都不能
授权，<kbd>Esc</kbd> 会停止当前回合。`h/j/k/l`、Tab 和 `y/n/c` 只保留在线性兼容
选择器中。

<p align="center">
  <img src="docs/assets/dsh-approval.png" alt="dsh-rs 补丁审批选择器，默认选中 Reject，可移动到 Allow once" width="1128">
</p>

<p align="center"><sub>这也是 Phase 9 旧版选择器截图；Phase 11 增强审批截图将在安装版截图门禁完成后替换。</sub></p>

### 一次性脚本调用

```console
dsh --workspace . --prompt '概括这个项目的目录结构'
printf '读取 README.md 并概括当前限制\n' | dsh --workspace .
```

脚本模式不会等待人工审批，因此文件写入、Shell 和插件调用会被拒绝。成功完成时，stdout
只输出最终提交的 assistant 文本，适合接入普通 Shell 流水线。

### 查看帮助

```console
dsh --help
```

主要参数包括 `--workspace`、`--model`、`--prompt`、`--list-sessions`、
`--resume`、`--plugin-config`、`--tui` 和 `--no-color`。

### 配置

`dsh` 当前使用少量环境变量，没有自动发现的项目配置或 profile 系统。唯一可选的
配置文件是显式传入的本地工具插件清单：

| 变量 | 用途 |
| --- | --- |
| `DEEPSEEK_API_KEY` | 必需的 DeepSeek API Key；每次请求时读取 |
| `DEEPSEEK_BASE_URL` | 可选的可信 API 地址；只允许 HTTPS，离线测试可用环回 HTTP |
| `DSH_SESSION_ROOT` | 可选的绝对会话目录，适合测试或运维隔离 |
| `XDG_STATE_HOME` | Linux 未设置会话目录覆盖时的状态目录基准 |
| `NO_COLOR` | 存在即关闭颜色并选择线性界面；`TERM=dumb` 和 `--no-color` 同样如此 |

默认会话目录是 macOS 的 `~/Library/Application Support/dsh/sessions`，以及 Linux
的 `$XDG_STATE_HOME/dsh/sessions`；未设置 `XDG_STATE_HOME` 时使用
`~/.local/state/dsh/sessions`。自定义 endpoint 不跟随重定向，也不使用系统代理。
更多示例见 [配置说明](docs/configuration.md)。

### 本地工具插件（实验性）

Phase 10 提供一个小而封闭的子进程工具接口。先构建仓库自带的两个无副作用示例，
再按照 [配置说明](docs/configuration.md#local-subprocess-tool-plugins) 创建私有 JSON：

```console
cargo +1.85.0 build --locked --examples
dsh --workspace . --plugin-config /absolute/path/to/plugins.json
```

插件是当前用户主动配置并运行的本地原生程序，**不是沙箱**。传入配置即允许它在
启动时运行并声明工具参数说明（schema）；交互模式下每次真正调用仍会弹出确认，
脚本模式则直接拒绝。
配置和可执行文件路径不会写入 Session，恢复时也不会自动继承，必须再次显式传
`--plugin-config`。首版只支持工具，不支持 Hook、Provider/Session 替换、MCP、npm、
热重载或通用 RPC。

## 会话与长对话

新的交互式会话会写入私有、有大小上限、只追加的本地 JSONL 日志。正常退出后可以
列出并恢复：

```console
dsh --list-sessions
dsh --list-sessions --workspace .
```

从列表复制一个会话 ID 后继续：

```console
dsh --resume session-550e8400-e29b-41d4-a716-446655440000
dsh --resume session-550e8400-e29b-41d4-a716-446655440000 \
  --prompt '继续上一项工作'
```

不传 `--workspace` 时，`dsh` 使用日志中已经验证过的原工作区；不传 `--model` 时，
沿用最近记录的模型。损坏或不支持的历史会在新的模型请求或工具副作用前失败，结果
不确定的旧工具调用不会被自动重放。

当已经提交的上下文达到模型窗口约 80%，或下一次请求已经装不下时，`dsh` 会先裁剪
过大的旧工具结果，再最多调用模型一次，将较早且工具调用/结果配对完整的前缀压成摘要。
它会保留最近约 16% 的完整上下文，然后继续同一条用户输入。空摘要、工具调用、失败
响应或没有真正缩短上下文的摘要都不会替换原对话。

> [!NOTE]
> 会话日志是正常退出后的便利性恢复，不是数据库、加密保险箱或备份。断电、
> `SIGKILL`、磁盘或文件系统故障可能丢失最后一段记录，或使该会话无法恢复。

## 安全边界

| 边界 | `dsh` 的做法 |
| --- | --- |
| 文件访问 | 文件工具只接受启动工作区内经过规范化和权限检查的路径，并拒绝已知的路径逃逸和危险链接 |
| 修改与执行 | 文件修改和 Shell 默认要求交互式审批；脚本模式直接拒绝这些副作用 |
| Shell | 获批的 Bash 是当前用户权限下的原生程序，**不是沙箱**，可以离开工作区、访问网络或修改其他文件 |
| 插件 | 显式配置的插件在启动时作为当前用户运行；环境最小化、协议和进程组有界，但审批不是系统隔离 |
| 秘密 | `DEEPSEEK_API_KEY` 不会被有意写入日志或终端输出；用户主动放入提示词、参数或命令的秘密仍然可见 |
| 资源 | 输入、流、工具输出、事件和会话有明确上限；常规 Shell 超时或取消会尝试终止同组进程 |
| 恢复 | 未知结果的旧工具调用不会自动重跑；损坏或不支持的历史不会被当作正常会话继续 |

详细报告与漏洞反馈方式见 [Security policy](SECURITY.md)。

无法中断的内核调用、主动逃离进程组的后裔或执行后的权限变化，仍可能延迟或阻止
Shell 清理；`dsh` 不把这些情况描述成沙箱保证。

## 项目状态

| 项目 | 状态 |
| --- | --- |
| 当前版本 | `0.1.0-alpha.0`，预发布 |
| Phase 0–9 | 已完成：v0.1 源码安装候选、终端体验、离线验收和双平台矩阵均已通过 |
| Phase 10 | 已完成：受限的本地子进程工具插件、两个真实示例和故障矩阵已通过双平台验收 |
| Phase 11 | 进行中：语义投影、Unicode Composer、下一回合 FIFO、inline Dock 和增强审批已有真实路径；产品卡片/视图、截图与双平台验收未完成 |

当前候选已通过本地 macOS arm64 验收，以及 GitHub-hosted `macos-14` arm64 和
`ubuntu-24.04` x86_64 的完整仓库检查、v0.1 安装版旅程与插件安装版旅程。Windows
和其他平台尚未实现或声明支持。

### 已知限制

- 当前只支持从源码安装，包没有发布到 crates.io，也没有预编译下载或 Homebrew formula；
- Provider 只有 DeepSeek；不支持 MCP、Hooks、Skills、子智能体或后台任务；
- 插件只支持显式本地工具子进程，不兼容 Cordis/npm，也不提供沙箱或热重载；
- `apply_patch` 一次只处理一个文件，Shell 只运行有界的前台命令，且获批 Shell 不是沙箱；
- 会话恢复面向正常退出后的继续工作，不是数据库级持久化或备份；
- 自动压缩每个 turn 最多尝试一次摘要，不保证摘要无损或事实完美；
- Phase 11 增强界面尚无 Markdown/diff 专用渲染、Focus/Inspect/Review、主题、Session picker 或最终安装版截图；
- Auto 暂不在 tmux、GNU Screen、Zellij、未知终端或初始小于 44×12 的窗口启用增强界面；已进入增强模式后可缩到 12×5，继续缩小会安全恢复并退出；
- primary-screen resize/reflow/copy 目前只有确定性终端模型和 PTY 字节证据，真实 iTerm/Terminal/VS Code 矩阵仍待完成；
- Windows 以及未列入发布矩阵的 Unix 平台尚未支持。

查看完整的 [Roadmap](docs/roadmap.md) 和逐项的
[Compatibility matrix](docs/compatibility.md)。

## 开发与验证

验证中的测试本身完全离线：它使用假模型、环回 HTTP 服务、临时工作区和明显的假密钥，
不会访问真实 DeepSeek API、读取你的 API Key、消耗额度或修改你的真实项目。第一次
构建仍可能从 crates.io 下载锁定的 Rust 依赖。

```console
./scripts/verify.sh
./scripts/accept-phase9.sh
./scripts/accept-phase10.sh
```

第一条执行格式、全部目标/feature 编译、测试、Clippy（warnings denied）和空白检查；
第二条安装 release 二进制到临时目录，再用真实 PTY 和离线 loopback Provider 跑完整
v0.1 发布旅程；第三条用同一安装方式运行两个真实插件示例和故障/取消/恢复矩阵。
安装阶段在本机尚未缓存依赖时可能访问 Cargo registry，但 Agent 场景不会调用真实
DeepSeek。三条命令也会在发布 CI 中运行。贡献前请阅读
[Contributing guide](CONTRIBUTING.md)。

## 上游关系

本项目不隶属于 DeepSeek、Anthropic 或 Claude Code。DeepSeek Harness 是固定的行为
参考，而不是品牌或发行关系：

- 上游仓库：<https://github.com/deepseek-ai/deepseek-harness>
- 固定基准：[`47f943859bef60e4160492346772ded9b24f765a`](https://github.com/deepseek-ai/deepseek-harness/tree/47f943859bef60e4160492346772ded9b24f765a)
- 研究记录：[docs/upstream.md](docs/upstream.md)

## 文档

- [产品路线图](docs/roadmap.md)
- [兼容性矩阵](docs/compatibility.md)
- [配置说明](docs/configuration.md)
- [发布检查表](docs/releasing.md)
- [Phase 8 验收记录](docs/validation/phase-8.md)
- [Phase 9 验收记录](docs/validation/phase-9.md)
- [Phase 10 插件设计](docs/design/subprocess-tool-plugins.md)
- [Phase 10 验收进度](docs/validation/phase-10.md)
- [贡献指南](CONTRIBUTING.md)
- [安全策略](SECURITY.md)

## License

本项目采用 [MIT License](LICENSE)。
