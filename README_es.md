# saluki

<!-- hy-mt2-i18n:start -->
[English](./README.md) | [中文](./README_zh-CN.md) | [日本語](./README_ja.md) | **Español**
<!-- hy-mt2-i18n:end -->


[![Licencia](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/DataDog/saluki/blob/main/LICENSE)

Saluki es una herramienta para crear planos de datos de telemetría en Rust.

## Estructura

Todo lo que se encuentra bajo `lib/` contiene código reutilizable o común, mientras que todo lo que está en `bin/` incluye paquetes dedicados a
crear binarios específicos para aplicaciones.

### Binarios

- `bin/agent-data-plane`: el binario principal del plano de datos, que proporciona un pipeline DogStatsD de nivel profesional y un
  pipeline OTLP experimental
- `bin/correctness`: binarios utilizados para ejecutar pruebas de corrección contra ADP y DogStatsD independiente

### Bibliotecas

El directorio `lib/` contiene dos grupos de paquetes:

**Reutilizables y de uso general** — Implementaciones de funcionalidades necesarias para Saluki o el Agente de Datos
Plane, pero que no son específicas de Saluki. Ejemplos incluyen `ddsketch`, código generado a partir de definiciones de Protocol Buffers, entre otros.

**Saluki** (`saluki-*`) — Paquetes fundamentales que componen a Saluki mismo, abarcando la construcción de topologías, rasgos de componentes,
primitivas de E/S, resolución de contexto, configuración y más.

## Contribuciones

Si encuentra un problema con este paquete y tiene una solución, o simplemente desea informarlo, por favor revise nuestra
guía [de contribuciones](https://datadoghq.dev/saluki/development/contributing).

## Documentación

La documentación procedural —arquitectura, lanzamientos, etc.— puede encontrarse [aquí](https://datadoghq.dev/saluki/).

## Seguridad

Si cree haber encontrado una vulnerabilidad de seguridad, consulte nuestra [Política de Seguridad](SECURITY.md).
