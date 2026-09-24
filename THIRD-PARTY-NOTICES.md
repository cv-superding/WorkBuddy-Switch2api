# Third-party notices

本仓库以 [MIT 许可](./LICENSE)发布。下列项目的版权声明与许可条款一并适用。

## changexbc/workbuddy-switch

- 仓库：https://github.com/changexbc/workbuddy-switch
- 许可：MIT
- 说明：账号切换、积分与 Token 统计、自动签到、自动轮换等绝大部分功能的上游作者。
  本仓库在此之上做了改造与扩展（API 反代、账号分组、反代用量统计、国内版 / 国际版双档位）。

## Harvey-Will/workbuddy-tools

- 仓库：https://github.com/Harvey-Will/workbuddy-tools
- 许可：MIT
- 说明：国际版（WorkBuddy AI）的数据目录划分与客户端 `account-snapshot.json`
  切换机制参考其 `core/editions.py` 与 `core/accounts.py`。相关代码为**独立重写**，
  未复制其源码。

## 其它依赖

运行时与构建依赖（Tauri、React、axum、reqwest、rusqlite 等）各自遵循其自身许可，
完整清单见 `Cargo.lock` 与 `package-lock.json`。

---

> **关于本文件的必要性**：第三方声明**不写在 `LICENSE` 里**。
> GitHub 的许可证识别器要求 `LICENSE` 是标准许可正文，在正文中间插入自定义段落
> 会让整个仓库被判定为 `Other` / `NOASSERTION`（而不是 MIT）。
> 所以第三方声明放在本文件，由 README 的「许可」一节引用。
