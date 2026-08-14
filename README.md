# deepseek-harness-cli-rs

DeepSeek Harness 核心能力的 Rust CLI 实现项目。

## 当前状态

当前可执行文件真实可用的行为只有：

- `dsh --help`：显示现有命令帮助；
- `dsh --version`：显示当前版本；
- 对缺少参数或未知参数返回非零退出码和清楚的错误信息。

项目内部已经实现并测试了六层 Rust 核心：只追加会话/回放、DeepSeek 流式 Provider、有步骤/重试/时间/资源上限并可取消的 Agent Loop、限定在启动工作区内的 `list`、`glob`、`grep`、`read` 只读工具、需要明确策略或一次性审批才能提交的严格单文件 `apply_patch`，以及受审批、超时、取消、输出上限和进程组清理保护的前台 `bash`。macOS 和 Ubuntu 24.04 上的 `LocalToolRegistry` 已通过验收，可以通过公共 Rust 接口交给 Agent 调用这些工具。默认文件写入和 Shell 策略都会询问审批；没有审批界面时会安全拒绝，不会静默修改文件或启动进程。

这些内部能力尚未接入 `dsh` 可执行文件。当前 CLI 仍只有帮助、版本和参数错误处理；它不能发起 DeepSeek API 请求、不能在终端中询问审批，也不能读写项目或执行 Shell。会话持久化和交互式终端也尚未实现，因此还不能用当前 `dsh` 进行 AI 编程对话。

## 构建与验证

仓库固定使用 Rust 1.85.0。安装 [Rustup](https://rustup.rs/) 后运行：

```console
cargo build --locked
cargo run -- --help
./scripts/verify.sh
```

`verify.sh` 会运行格式检查、编译、测试和 Clippy。默认验证不会访问网络、读取 API Key 或消耗模型额度。

## 项目关系

本项目是独立的社区开源项目，不隶属于 DeepSeek、Anthropic 或 Claude Code。

DeepSeek Harness 是本项目固定参考的上游实现：

- 仓库：<https://github.com/deepseek-ai/deepseek-harness>
- 基准 commit：[`47f943859bef60e4160492346772ded9b24f765a`](https://github.com/deepseek-ai/deepseek-harness/tree/47f943859bef60e4160492346772ded9b24f765a)

## License

本项目采用 [MIT License](LICENSE)。
