# deepseek-harness-cli-rs

DeepSeek Harness 核心能力的 Rust CLI 实现项目。

## 当前状态

`dsh` 现在已经是一条真实可用的、会调用 DeepSeek 的终端 Agent 路径。它可以：

- 在同一个进程内连续进行多轮对话，并实时显示已经提交到会话日志的模型文本；
- 使用限定在启动工作区内的 `list`、`glob`、`grep`、`read` 和严格单文件
  `apply_patch` 工具；
- 运行前台 `bash`，并在超时、取消或输出过大时清理它拥有的进程组；
- 在文件修改或 Shell 执行前显示完整预览，并要求输入终端上显示的
  `allow <一次性编号>`；也可以输入 `reject` 或 `cancel`；
- 用 Ctrl+C 取消当前回合并继续会话，用 Ctrl+D 安全退出，用 Ctrl+Z 先清理当前
  回合再暂停，回到 Shell 后可用 `fg` 恢复；
- 通过 `--prompt` 或管道输入运行一次脚本化请求。脚本模式会安全拒绝文件写入和
  Shell，不会停下来等待人工审批。

终端界面故意保持为朴素的逐行文本，不使用全屏、ANSI 颜色或 raw mode。新的 CLI 会话
现在会写入私有、有上限、只追加的本地 JSONL 日志，方便正常退出后通过
`--list-sessions` 和 `--resume` 继续工作。这是便利性保存，不是数据库或备份保证；
断电、`SIGKILL`、磁盘或文件系统故障可能丢失最后一段记录，或使会话无法恢复。
`dsh` 遇到损坏或不支持的历史会在新的模型请求或工具执行前失败，也不会自动
重跑结果不确定的旧工具调用。重要代码修改应保存在工作区并使用 Git 管理。
当已经提交的上下文达到模型窗口约 80%，或下一次请求已经装不下时，`dsh` 会先裁剪
过大的旧工具结果，再最多调用模型一次，把较早且工具调用/结果配对完整的前缀压成摘要，
保留最近约 16% 的完整上下文，并继续处理同一条用户输入。空摘要、工具调用、失败响应或
没有真正缩短上下文的摘要不会替换原对话；同一个回合也不会反复摘要。
旧的 4,096 事件内存生命周期上限不再限制这条持久化 CLI 路径。获批的 Shell 是当前用户
权限下的原生程序，不是安全沙箱；它可以访问工作区外资源，因此只应批准你理解的命令。
当前 macOS 终端路径已完成本地验收；Ubuntu 24.04
也已通过同一固定提交的远程 CI 验收。其他 Linux 发行版、Windows 和其他系统尚未声明
支持。

## 快速开始

仓库固定使用 Rust 1.85.0。先安装 [Rustup](https://rustup.rs/)，然后执行：

```console
cargo build --locked
export DEEPSEEK_API_KEY='你的 DeepSeek API Key'
cargo run --locked -- --workspace .
```

看到 `dsh >` 后直接输入问题并按回车。终端内可用 `/help`、`/exit` 和 `/quit`。
一次性脚本调用示例：

```console
cargo run --locked -- --workspace . --prompt '概括这个项目的目录结构'
printf '读取 README.md 并概括当前限制\n' | cargo run --locked -- --workspace .
```

只列出已经持久化的会话头（会话 ID、Unix 毫秒创建时间和安全转义的工作区路径）：

```console
cargo run --locked -- --list-sessions
cargo run --locked -- --list-sessions --workspace .
```

从列表复制一个规范的会话 ID 后继续对话；不传 `--workspace` 时使用日志中已经验证过的
原工作区，不传 `--model` 时沿用最近一次记录的模型：

```console
cargo run --locked -- --resume session-550e8400-e29b-41d4-a716-446655440000
cargo run --locked -- --resume session-550e8400-e29b-41d4-a716-446655440000 \
  --prompt '继续上一项工作'
```

API Key 只从进程环境按请求读取，不会有意写入会话或终端输出。不要把真实密钥放进
提示词、工具参数或 Shell 命令，因为这些内容本来就是模型和会话可见的。

## 构建与验证

默认验证完全离线：它使用假模型、环回 HTTP 服务、临时工作区和明显的假密钥，不会
访问真实 DeepSeek API、读取你的 API Key 或消耗额度。

```console
cargo run -- --help
./scripts/verify.sh
```

## 项目关系

本项目是独立的社区开源项目，不隶属于 DeepSeek、Anthropic 或 Claude Code。

DeepSeek Harness 是本项目固定参考的上游实现：

- 仓库：<https://github.com/deepseek-ai/deepseek-harness>
- 基准 commit：[`47f943859bef60e4160492346772ded9b24f765a`](https://github.com/deepseek-ai/deepseek-harness/tree/47f943859bef60e4160492346772ded9b24f765a)

## License

本项目采用 [MIT License](LICENSE)。
