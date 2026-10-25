# 测试说明

应用、安装器、systemd 主服务和 NVIDIA 恢复钩子使用 Rust 二进制。Rust 单元测试覆盖控制策略、硬件接口、配置和 HTTP 协议；`tests/test_rust.py` 启动临时演示服务和模拟 NVML 子进程执行黑盒测试；`tests/browser-smoke.mjs` 验证浏览器工作流。

| 测试范围 | 覆盖位置 |
|---|---|
| 默认曲线、插值、输出范围、平均/回差/延迟/升降速限制 | `src/tests.rs` 的计算及策略测试 |
| 原始温度过热、缺失温度、主板停转保护、GPU 自身温度及零转速模式 | `src/tests.rs` + 模拟 NVML 进程测试 |
| 接管初始输出、保留原租约、停止恢复、恢复失败重试 | `src/tests.rs` |
| 后台重启、延迟出现设备、前台不自动接管、停止全部持久化 | `src/tests.rs` + `tests/test_rust.py` |
| 命名确认、临时识别、超时、单通道测试、泵保护、恢复原控制 | Rust 行为测试、真实子进程超时测试及浏览器测试 |
| 共享曲线、局部修改、并发冲突、排序、原子分配、磁盘保存失败 | Rust 单元、HTTP 黑盒和浏览器回归 |
| v1 原文件备份、损坏配置保留、参数校验 | Rust 配置测试及黑盒测试 |
| hwmon 原状态恢复、设备重编号、未知芯片只读 | 临时 sysfs 目录测试 |
| NVIDIA 写入前持久化、逐通道超时、旧令牌、恢复失败、EOF/SIGTERM/SIGKILL | Rust Writer 测试 + `tests/mock_nvml.c` 黑盒进程测试 |
| 安装预检、驱动检查、备份、配置开关、服务路径转义、前台锁冲突 | `src/install.rs` 中使用临时目录和命令记录器的测试 |
| HTTP Host/Origin/令牌、路径限制、增量响应 | Rust Host 测试 + `tests/test_rust.py` |
| 观看/编辑分离、隐藏页停止轮询、手机布局 | API 测试 + `tests/browser-smoke.mjs` |
| 健康后台不定期发现、编辑租约超时、故障发现退避、实时读取 | Rust 调度测试及临时 sysfs 测试 |

运行：

```bash
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
./build.sh
python3 -m unittest discover -s tests -v
node --check static/app.js
node --check static/view.js
bash -n install-service.sh webui.sh build.sh
```

回归测试覆盖滑动总和在缺失读数、窗口边界和来源修改后的正确性，以及请求头分块读取、正文保留、大小限制、截断与大小写重复。浏览器回归覆盖编辑到观看页的实际导航、曲线库滚动时创建/释放 SVG 且高度不变、传感器读数改变时保留行和文本节点。

索引回归覆盖设备重排、同数量设备替换、移除、温度分组变化和缺失成员。模拟 GPU 进程回归另外覆盖复用布局中的实时温度、模式、输出、RPM、读数失败与恢复，以及风扇数量变化和整卡消失/重连。浏览器回归验证文本节点复用、稳定输出条不写 DOM，以及大曲线库按焦点/指针展开和失焦收起后的保存行为。

黑盒 GPU 测试需要 C 编译器，动态库只注入测试子进程。安装单元测试不会调用真实 systemctl、安装驱动或修改系统目录。

浏览器回归使用临时演示服务，先验证默认观看页没有编辑控件、没有 POST 请求、不加载编辑脚本，再测试 `/edit` 的编辑工作流；截图保存在 `test-results/`。启动命令见 README。

状态快照回归另覆盖观看快照升级为编辑快照、同版本共享、旧读取期间修改配置、缺失读数、发现结果一致性和观看接口不泄漏令牌。浏览器回归覆盖两页的大量卡片释放/恢复 SVG、滚动高度不变和恢复后的编辑入口。
