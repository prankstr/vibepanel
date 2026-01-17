# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.2](https://github.com/prankstr/vibepanel/compare/v0.5.1...v0.5.2) (2026-01-17)


### Features

* add AUR and Copr packaging automation ([872ffc9](https://github.com/prankstr/vibepanel/commit/872ffc9878e80873a41196915635c7d46784ccdb))
* **quick_settings:** add per-row accordions and height-limited scroll ([1f7d6b4](https://github.com/prankstr/vibepanel/commit/1f7d6b4b13a9f1b007eb70a1ca7a529c5b8710ab))


### Bug Fixes

* **bar:** improve monitor hot-plug behaviour, no more flickering ([84ccd9d](https://github.com/prankstr/vibepanel/commit/84ccd9de0282bc8c36236ad486e1b99e97615bb3))
* **quick_settings:** hide brightness when unavailable ([3bbb869](https://github.com/prankstr/vibepanel/commit/3bbb869172bd0a76f2850a8aabee1ad3e0fb8c09))

## [0.5.1](https://github.com/prankstr/vibepanel/compare/v0.5.0...v0.5.1) (2026-01-16)


### Bug Fixes

* better battery icon thresholds ([ddb3277](https://github.com/prankstr/vibepanel/commit/ddb3277550d9f35a22e3adf68fb92c2b968a44d5))
* IconHandle CSS class tracking survives theme switches ([264de22](https://github.com/prankstr/vibepanel/commit/264de22cbb8c6d29ea197ec975137d69f1c0fd07))
* **readme:** identity crisis solved for now ([447d5ad](https://github.com/prankstr/vibepanel/commit/447d5adba147ed722f1d54ad8a30094262b82890))
* use Pango API for font registration instead of fontconfig FFI ([dfc7589](https://github.com/prankstr/vibepanel/commit/dfc75890191c82df56c68febaaf46e6b936c1230))

## [0.5.0](https://github.com/prankstr/vibepanel/compare/v0.4.0...v0.5.0) (2026-01-15)


### ⚠ BREAKING CHANGES

* window_title.format removed. Use template instead.
* [workspace] config section removed. Use [advanced].compositor instead.
* **css:** CSS class renamed from `.notification` to `.notifications`

### Features

* **css:** improve CSS customizability for Quick Settings and surfaces ([0a72720](https://github.com/prankstr/vibepanel/commit/0a72720a516516f5fb63fcc81de2611465ba8e23))


### Bug Fixes

* **ci:** pass tag_name to release workflow to fix GitHub Releases ([c87e024](https://github.com/prankstr/vibepanel/commit/c87e02447645899a6e511747f62345ee5de22c19))
* **css:** quick settings window inherits widget color ([0a72720](https://github.com/prankstr/vibepanel/commit/0a72720a516516f5fb63fcc81de2611465ba8e23))
* **css:** rename notification widget class to notifications ([0a72720](https://github.com/prankstr/vibepanel/commit/0a72720a516516f5fb63fcc81de2611465ba8e23))
* **css:** use CSS variable for popover background ([0a72720](https://github.com/prankstr/vibepanel/commit/0a72720a516516f5fb63fcc81de2611465ba8e23))
* **css:** use CSS variable for surface text color ([0a72720](https://github.com/prankstr/vibepanel/commit/0a72720a516516f5fb63fcc81de2611465ba8e23))
* **docs:** remove undocumented package_manager option from updates widget ([1927090](https://github.com/prankstr/vibepanel/commit/1927090dd0cf34bb8b6524344bc40ae848e4ab54))


### Code Refactoring

* move [workspace] config to [advanced].compositor ([1927090](https://github.com/prankstr/vibepanel/commit/1927090dd0cf34bb8b6524344bc40ae848e4ab54))
* remove window_title.format option ([1927090](https://github.com/prankstr/vibepanel/commit/1927090dd0cf34bb8b6524344bc40ae848e4ab54))

## [0.4.0](https://github.com/prankstr/vibepanel/compare/v0.3.0...v0.4.0) (2026-01-15)


### ⚠ BREAKING CHANGES

* Config schema has changed. The following options have moved: Section Moves

### Bug Fixes

* **workspace:** support multi-tag view in Mango/DWL workspace widget ([#11](https://github.com/prankstr/vibepanel/issues/11)) ([54f3d65](https://github.com/prankstr/vibepanel/commit/54f3d6527b6be2590093ab120c6111a49d883dcf))


### Code Refactoring

* reorganize config structure for more intuitive structure ([#9](https://github.com/prankstr/vibepanel/issues/9)) ([6c0172e](https://github.com/prankstr/vibepanel/commit/6c0172e2e7eeb11a76cf28a01ed04209b1e1fc8b))

## [0.3.0](https://github.com/prankstr/vibepanel/compare/v0.2.1...v0.3.0) (2026-01-14)


### ⚠ BREAKING CHANGES
* **config:** Section configuration has been simplified. The `center_left` and `center_right` sections have been removed. To place widgets adjacent to the notch with notch mode, use the regular left and right sections together with the new spacer widget.

### Features

* add per-widget background color configuration ([#5](https://github.com/prankstr/vibepanel/issues/5)) ([58c9be2](https://github.com/prankstr/vibepanel/commit/58c9be217bc40f669a64c04a528aedbe39df696a))
* configurable icon weight with live reload ([60c45fd](https://github.com/prankstr/vibepanel/commit/60c45fd9d84b68191386fc0b4584b6ff674cf78b))
* simplify section configuration and add spacer widget ([#7](https://github.com/prankstr/vibepanel/issues/7)) ([1fbcac2](https://github.com/prankstr/vibepanel/commit/1fbcac22754d1dfd8d7c36a00172e5e3d6826a26))


### Bug Fixes

* apply widget_opacity config to widget backgrounds ([#4](https://github.com/prankstr/vibepanel/issues/4)) ([b856257](https://github.com/prankstr/vibepanel/commit/b856257f699e99f2653d22c3ab7686e10a1a064d))
* center number labels in workspace indicator pills ([720a24a](https://github.com/prankstr/vibepanel/commit/720a24a33efddcf85205a885676f9d6d7bd70f1a))
* **ci:** use simple release type with custom jsonpath for workspace version ([2395bea](https://github.com/prankstr/vibepanel/commit/2395beaef9302cc09e83113a2e71e914d6c0f887))
* **css:** apply consistent radius to tooltips and popover menus ([5665300](https://github.com/prankstr/vibepanel/commit/56653009c8ec192aeee793e41582c094f63f096e))
* make accent text color respect dark/light mode ([2a947f3](https://github.com/prankstr/vibepanel/commit/2a947f31c869640aedf515244a2f883e136510ae))
* make tooltips slightly transparant ([bf4ce98](https://github.com/prankstr/vibepanel/commit/bf4ce98de27f02ca2aab1b46b4b80055ed5fb451))
* restore notification toast truncation and improve stacking ([#6](https://github.com/prankstr/vibepanel/issues/6)) ([e1d5f79](https://github.com/prankstr/vibepanel/commit/e1d5f791c8a3a96eed3a647b26e59e8bc07db548))
* unify group island background color ([474239a](https://github.com/prankstr/vibepanel/commit/474239a4e259333b5e763d096a8ea0c70dd11c00))

## [0.2.1] - 2025-01-07

### Fixed

- Calendar CSS syntax error causing GTK theme parser warnings

## [0.2.0] - 2025-01-07

### Added

- Support for markup in notifications, allowing rich text formatting
- Calendar week header display
- Settings option to disable calendar weeks

### Changed

- CI optimization improvements

## [0.1.1] - 2024-12-30

### Fixed

- Notification text now truncates on character boundaries instead of bytes, preventing multibyte characters (e.g., åäö) from being split
- Password input in WiFi quick settings panel
- Truncation of subtitles in toggle cards

## [0.1.0] - Initial Release

- Initial release of vibepanel
