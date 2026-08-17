<div align="center">
  <h1><code>dsh-rs</code></h1>
  <p><strong>用 Rust 构建的 DeepSeek 终端编程 Agent</strong></p>
  <p>在真实代码仓库里持续对话：搜索和阅读代码、应用补丁、运行命令，并在长会话中保存、恢复与压缩上下文。</p>
  <p>
    <a href="https://github.com/xizheyin/deepseek-harness-cli-rs/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/xizheyin/deepseek-harness-cli-rs/actions/workflows/ci.yml/badge.svg"></a>
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
> `dsh` 当前是 `0.1.0-alpha.0` 预发布版本，尚无受支持的正式发行版。Phase 9
> 正在进行发布前的终端体验、安装和端到端验收。

`dsh-rs` 是项目名，安装后的命令是 `dsh`。这是一个独立的社区开源项目，Agent 内核以固定版本的
[DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) 为行为参考，
再用适合 Rust CLI 的类型、并发模型和安全边界重新实现；它不是官方产品，也不是
TypeScript 源码的逐行翻译。

## 快速开始

仓库固定使用 Rust 1.85.0。安装 [Rustup](https://rustup.rs/) 后，在仓库根目录执行：

```console
cargo build --locked
export DEEPSEEK_API_KEY='你的 DeepSeek API Key'
cargo run --locked -- --workspace .
```

看到 `dsh >` 后，直接输入任务并按回车。例如：

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
| 交互控制 | 多轮对话、实时状态、审批、Ctrl+C 取消当前回合，以及干净的 EOF/暂停处理 |
| 脚本模式 | `--prompt` 或管道输入；不会停下来等待审批，并安全拒绝写文件或 Shell 请求 |
| 长会话 | 有上限的本地 JSONL、会话列表与恢复，以及一次有界的自动上下文摘要 |

## 使用方式

### 交互模式

```console
cargo run --locked -- --workspace .
```

| 输入 | 行为 |
| --- | --- |
| `/help` | 显示会话内帮助 |
| `/exit` 或 `/quit` | 等待清理后退出 |
| <kbd>Ctrl</kbd> + <kbd>C</kbd> | 取消当前回合，清理完成后继续当前会话 |
| <kbd>Ctrl</kbd> + <kbd>D</kbd> | 安全结束会话 |
| <kbd>Ctrl</kbd> + <kbd>Z</kbd> | 先清理当前回合再暂停；回到 Shell 后可用 `fg` 恢复 |

文件修改或 Shell 执行前会显示完整预览，并要求输入终端上显示的
`allow <一次性编号>`；也可以输入 `reject` 或 `cancel`。

### 一次性脚本调用

```console
cargo run --locked -- --workspace . --prompt '概括这个项目的目录结构'
printf '读取 README.md 并概括当前限制\n' | cargo run --locked -- --workspace .
```

脚本模式不会等待人工审批，因此文件写入和 Shell 调用会被拒绝。成功完成时，stdout
只输出最终提交的 assistant 文本，适合接入普通 Shell 流水线。

### 查看帮助

```console
cargo run --locked -- --help
```

主要参数包括 `--workspace`、`--model`、`--prompt`、`--list-sessions`、
`--resume` 和 `--no-color`。

## 会话与长对话

新的交互式会话会写入私有、有大小上限、只追加的本地 JSONL 日志。正常退出后可以
列出并恢复：

```console
cargo run --locked -- --list-sessions
cargo run --locked -- --list-sessions --workspace .
```

从列表复制一个会话 ID 后继续：

```console
cargo run --locked -- --resume session-550e8400-e29b-41d4-a716-446655440000
cargo run --locked -- --resume session-550e8400-e29b-41d4-a716-446655440000 \
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
| Phase 0–8 | 已完成：基础设施、Provider、Agent、工具、审批、Shell、终端、会话恢复与自动摘要 |
| Phase 9 | 进行中：终端体验、源码安装、完整离线验收、文档和发布矩阵 |
| Phase 10 | 计划中：受限的本地子进程工具插件；不属于 v0.1 |

当前开发和测试主要面向 macOS 与 Ubuntu 24.04。macOS 已有本地终端验收；Ubuntu
24.04 是 CI 和 v0.1 发布矩阵目标，当前 Phase 9 仍在重新验收。Windows 和其他平台
尚未实现或声明支持。

查看完整的 [Roadmap](docs/roadmap.md) 和逐项的
[Compatibility matrix](docs/compatibility.md)。

## 开发与验证

验证中的测试本身完全离线：它使用假模型、环回 HTTP 服务、临时工作区和明显的假密钥，
不会访问真实 DeepSeek API、读取你的 API Key、消耗额度或修改你的真实项目。第一次
构建仍可能从 crates.io 下载锁定的 Rust 依赖。

```console
./scripts/verify.sh
```

这是本地与 GitHub Actions 共用的验证入口，包含格式、全部目标/feature 编译、测试、
Clippy（warnings denied）和空白检查。贡献前请阅读 [Contributing guide](CONTRIBUTING.md)。

## 上游关系

本项目不隶属于 DeepSeek、Anthropic 或 Claude Code。DeepSeek Harness 是固定的行为
参考，而不是品牌或发行关系：

- 上游仓库：<https://github.com/deepseek-ai/deepseek-harness>
- 固定基准：[`47f943859bef60e4160492346772ded9b24f765a`](https://github.com/deepseek-ai/deepseek-harness/tree/47f943859bef60e4160492346772ded9b24f765a)
- 研究记录：[docs/upstream.md](docs/upstream.md)

## 文档

- [产品路线图](docs/roadmap.md)
- [兼容性矩阵](docs/compatibility.md)
- [Phase 8 验收记录](docs/validation/phase-8.md)
- [贡献指南](CONTRIBUTING.md)
- [安全策略](SECURITY.md)

## License

本项目采用 [MIT License](LICENSE)。
