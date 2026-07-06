[English](./README.md) | **中文**

# Recall

> 本地优先，搜索你机器上所有 AI 编程会话。

[![Recall](docs/recall.png)](https://asciinema.org/a/909453)

在 Claude Code、Codex 以及后续各种 CLI 之间跳转；Recall 将这些分散在本地的会话收进一个可搜索的索引，在可用时跟踪 token 用量，并把你送回原始 CLI。

## 安装

```bash
brew install samzong/tap/recall
```

## 用法

```bash
recall sync          # 增量同步（随时可安全运行）
recall               # 启动 TUI
recall usage         # 用量面板
recall export > recall-export.jsonl # 导出全部会话
recall import recall-export.jsonl --dry-run  # 预览导入
recall session list  # 为 agent/脚本列出会话
recall session share --id <session-id> --format json  # 发布选中的一个会话
recall info  # 索引统计与 worker 状态
```

配合 Skill 使用时，**Recall** 是最佳方式。

```bash
recall skill install # 自动检测 agent 并安装 skills
```

## 支持

一个索引覆盖所有 AI 编程 CLI。同步一次，处处搜索，从上次停下的地方继续。

| 适配器          | 发现 | 全量索引 | 增量同步 | 语义搜索 | 导出 | 恢复 | 用量 |
| --------------- | :--: | :------: | :------: | :------: | :--: | :--: | :--: |
| Claude Code     |  ✅  |    ✅    |    ✅    |    ✅    |  ✅  |  ✅  |  ✅  |
| OpenCode        |  ✅  |    ✅    |    ✅    |    ✅    |  ✅  |  ✅  |  ✅  |
| Codex           |  ✅  |    ✅    |    ✅    |    ✅    |  ✅  |  ✅  |  ✅  |
| Pi              |  ✅  |    ✅    |    ✅    |    ✅    |  ✅  |  ✅  |  ✅  |
| Antigravity     |  ✅  |    ✅    |    ✅    |    ✅    |  ✅  |  ✅  |      |
| Gemini          |  ✅  |    ✅    |    ✅    |    ✅    |  ✅  |  ✅  |  ✅  |
| Kiro            |  ✅  |    ✅    |    ✅    |    ✅    |  ✅  |  —   |      |
| Copilot         |  ✅  |    ✅    |    ✅    |    ✅    |  ✅  |  ✅  |      |
| Cursor          |  ✅  |    ✅    |    ✅    |    ✅    |  ✅  |  —   |  ✅  |
| Cline           |  ✅  |    ✅    |    ✅    |    ✅    |  ✅  |  —   |      |
| Grok            |  ✅  |    ✅    |    ✅    |    ✅    |  ✅  |  ✅  |      |

## 致谢

- 感谢 [tokscale](https://github.com/junhoyeo/tokscale) 提供的用量面板参考与 token 统计行为。
- 感谢 [Ratatui](https://github.com/ratatui/ratatui) 与 [Crossterm](https://github.com/crossterm-rs/crossterm) 提供的终端 UI 基础。
- 感谢 [sqlite-vec](https://github.com/asg017/sqlite-vec) 与 SQLite FTS5，让本地文本与向量搜索保持嵌入式。
- 感谢 [Candle](https://github.com/huggingface/candle)、Hugging Face 与 [intfloat/multilingual-e5-small](https://huggingface.co/intfloat/multilingual-e5-small) 提供的本地语义嵌入。
- 感谢 [kitup](https://github.com/samzong/kitup) 提供的内置 agent skill 安装器。
- 感谢 [LINUX DO](https://linux.do/) 开源分享社区。

## 许可证

本项目采用 [MIT](LICENSE) 许可证。
