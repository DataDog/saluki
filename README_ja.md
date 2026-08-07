# saluki

<!-- hy-mt2-i18n:start -->
[English](./README.md) | [中文](./README_zh-CN.md) | **日本語** | [Español](./README_es.md)
<!-- hy-mt2-i18n:end -->


[![ライセンス](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/DataDog/saluki/blob/main/LICENSE)

Salukiは、Rustを使ってテレメトリデータプレーンを構築するためのツールキットです。

## 構成

`lib/`以下にあるものはすべて再利用可能な共通コードであり、`bin/`以下にあるものはアプリケーション固有のバイナリを
構築するための専用クレートが含まれています。

### バイナリ

- `bin/agent-data-plane`：主要なデータプレーンバイナリで、本番環境向けのDogStatsDパイプラインと実験的なOTLPパイプラインを提供します。
- `bin/correctness`：ADPおよびスタンドアロンのDogStatsDに対して正確性テストを実行するためのバイナリです。

### ライブラリ

`lib/`ディレクトリには2つのグループのクレートが含まれています：

**再利用可能で汎用的なもの** — Salukiやエージェントデータプレーンに必要だがSaluki特有ではない機能や能力の実装です。例としては`ddsketch`やProtocol Buffers定義用の生成コードなどがあります。

**Saluki** (`saluki-*`) — Saluki自体を構成する基盤となるクレートで、トポロジ構築、コンポーネントのトレイト、I/Oプリミティブ、コンテキスト解決、設定などを扱っています。

## 貢献方法

このパッケージに問題を見つけて修正案がある場合や、単に報告したいだけの場合は、ぜひ当社の
[貢献ガイド](https://datadoghq.dev/saluki/development/contributing)をご覧ください。

## ドキュメント

アーキテクチャやリリース方法などの手順系ドキュメントは、[こちら](https://datadoghq.dev/saluki/)で確認できます。

## セキュリティ

セキュリティ上の脆弱性を発見したと思われる場合は、当社の[セキュリティポリシー](SECURITY.md)をご参照ください。
