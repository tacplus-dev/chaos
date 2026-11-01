# Chaos Mesh Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [Unreleased]

### Added

- Nothing

### Changed

- Nothing

### Fixed

- Nothing

## [9.9.9] - 2026-11-01

### Added

- Add PodChaos action pod-freeze to stop a pod's processes [#9999](https://github.com/chaos-mesh/chaos-mesh/pull/9999)
- Support `serviceAccountName` for Workflow Task pods [#5162](https://github.com/chaos-mesh/chaos-mesh/pull/5162)

### Changed

- Nothing

### Deprecated

- Nothing

### Removed

- Nothing

### Fixed

- Fix PodChaos pod-kill retries on terminating pods [#9998](https://github.com/chaos-mesh/chaos-mesh/pull/9998)

### Security

- Nothing

## [2.8.7] - 2026-10-28

### Added

- Support `serviceAccountName` for Workflow Task pods [#5162](https://github.com/chaos-mesh/chaos-mesh/pull/5162)
- Support `nodeSelector` and `tolerations` for workflow task pods [#4790](https://github.com/chaos-mesh/chaos-mesh/pull/4790)

### Fixed

- Fix JVMChaos preparation failures in containers that use Rust coreutils [#5161](https://github.com/chaos-mesh/chaos-mesh/pull/5161)
