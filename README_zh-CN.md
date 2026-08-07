# saluki

<!-- hy-mt2-i18n:start -->
[English](./README.md) | **中文** | [日本語](./README_ja.md) | [Español](./README_es.md)
<!-- hy-mt2-i18n:end -->


[![许可证](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/DataDog/saluki/blob/main/LICENSE)

Saluki 是一个用于用 Rust 构建遥测数据平面的工具包。

## 结构

`lib/` 目录下的所有内容均为可复用/通用代码，而 `bin/` 目录下的内容则是用于构建特定应用二进制文件的专用 crate。

### 二进制文件

- `bin/agent-data-plane`：主要的数据平面二进制文件，它提供了生产级嘅 DogStatsD 流处理管道以及一个实验性的 OTLP 流处理管道
- `bin/correctness`：用于针对 ADP 和独立运行的 DogStatsD 执行正确性测试的二进制文件

### 库

`lib/` 目录包含两类 crate：

**可复用且通用型**——实现 Saluki 或 Agent Data Plane 所需的功能/能力，但这些功能并非 Saluki 所独有。例如 `ddsketch`、Protocol Buffers 定义生成的代码等等。

**Saluki**（`saluki-*`）——构成 Saluki 本身的基础 crate，涵盖拓扑结构构建、组件特性、I/O 原语、上下文解析、配置设置等功能。

## 贡献代码

如果您发现该软件包存在问题并已有修复方案，或者只是想进行问题报告，请查阅我们的[贡献指南](https://datadoghq.dev/saluki/development/contributing)。

## 文档

关于架构、发布等相关流程的文档可在此处查看：[这里](https://datadoghq.dev/saluki/)。

## 安全性

如果您认为发现了安全漏洞，请参阅我们的[安全政策](SECURITY.md)。
