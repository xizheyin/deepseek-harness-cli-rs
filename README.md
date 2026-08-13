# deepseek-harness-cli-rs

DeepSeek Harness 核心能力的 Rust CLI 实现项目。

## 当前状态

当前可执行文件真实可用的行为只有：

- `dsh --help`：显示现有命令帮助；
- `dsh --version`：显示当前版本；
- 对缺少参数或未知参数返回非零退出码和清楚的错误信息。

项目内部已经实现并测试了与固定 DeepSeek Harness 版本对照的 Rust 会话核心，包括消息/工具类型、只追加事件、回放、turn/step/tool 关系和模型上下文投影。它尚未接入 `dsh` 可执行文件。

DeepSeek API、Agent Loop、文件工具、Shell、会话持久化和交互式终端尚未实现；目前不能用它进行 AI 编程对话。

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
