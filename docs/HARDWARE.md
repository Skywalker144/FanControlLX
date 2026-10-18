# 本机硬件接入记录

记录日期：2026-10-18。

## 已确认

- 主机：sky-5080。
- 主板：Micro-Star International MAG B860M MORTAR WIFI (MS-7E40)。
- 系统：Ubuntu 24.04.4 LTS；本次运行内核为 7.0.0-38-generic。
- GPU：NVIDIA GeForce RTX 5080，驱动 595.99.02。正常宿主机会话可读取温度。
- 内核日志：`nct6683: Found NCT6687D or compatible chip at 0x4e:0xa20`。
- 内核自带 nct6683 模块加载后，没有注册出可用的主板 hwmon/PWM 设备。用户已将其临时卸载。
- 当前未安装 coolercontrold 服务。

## 已加载的驱动

上游：[Fred78290/nct6687d](https://github.com/Fred78290/nct6687d)。

构建时固定的源码提交：

```text
060728e8d04e3ee17b5e003a53e0bcb34c60de92
```

源码下载位置：`/tmp/fancontrollx-nct6687d`。源码匹配表明确包含 `MAG B860M MORTAR WIFI (MS-7E40)`，采用 MSI 替代风扇寄存器布局。

本地构建结果：

```text
/home/sky/RL/FanControlLX/.runtime/nct6687d/nct6687.ko
```

签名后的加载文件为同目录 `nct6687.signed.ko`。签名者为 `sky-5080 Secure Boot Module Signature key`。

许可证副本位于同目录 `LICENSE`。构建产物属于该驱动的 GPL-2.0 代码，未纳入应用源码追踪。

该二进制只针对当前 7.0.0-38-generic 内核，更新内核后需要重新构建。临时目录可能在重启后被清理；源码可从固定提交重新获取。

最初尝试加载未签名模块时返回 `Key was rejected by service`。随后已生成 MOK、完成模块签名，并在重启后的 MOK 界面登记证书。`mokutil --test-key /var/lib/shim-signed/mok/MOK.der` 已确认 `is already enrolled`。

已成功执行的临时加载命令：

```bash
sudo insmod /home/sky/RL/FanControlLX/.runtime/nct6687d/nct6687.signed.ko msi_fan_brute_force=1
```

用户提供的诊断已确认：

- `nct6687` 已注册到 `/sys/class/hwmon/hwmon6`，稳定设备 ID 为 `67643fd4cc804986`。
- 暴露 `pwm1` 至 `pwm8`，均处于模式 2（固件自动控制），均暴露看门狗接口。
- `fan1` / CPU Fan：1066 RPM；`fan2` / Pump Fan：789 RPM；`fan6` / System Fan #4：1595 RPM。这是诊断时的快照。
- 其余 NCT 风扇通道为 0 RPM；实际接线尚需核实。
- CPU、System、VRM MOS、PCH、CPU Socket 温度均有读数。PCIe x1 和 M2_1 读数为 0，不宜在核实前用作控制温度源。
- GPU 读取正常。
- 诊断 UID 为 1000，通道不可写，提示通过 sudo 启动控制模式；这不表示驱动加载失败。

以上是首次加载时的诊断快照。后续用户完成识别并安装了后台服务，当前状态见下节。未安装 DKMS，未修改内核启动参数。

## 后台接入结果

2026-10-18 安装 `fancontrollx.service` 后的只读检查：

- systemd 状态为 `active/running`，开机启动为 `enabled`，异常重启计数为 0。
- CPU Fan 使用 pwm1/fan1；CaseOut 使用 pwm2/fan2；CaseIn 使用 pwm6/fan6。三路均已由用户确认身份并启用持续控制，模式为温度曲线。
- CPU Fan 使用 CPU 温度；CaseOut、CaseIn 使用 CPU/GPU 最高温度。
- 运行配置与 `data/config.json` 一致，三路均显示正在控制，无故障或待接管项目。
- 一次连续采样时三路计算输出约 34.4%，PWM 回读 34.5%（8 位 PWM 量化），转速分别约 1110、1047、1044 RPM。温度和转速会随负载变化；回差和时间平均也会影响即时输出。
- 已签名模块安装在当前内核的模块目录，服务启动前会自动加载模块。

本次没有改变曲线、停止服务、模拟故障或重启机器；实际重启恢复、异常退出看门狗及完整温度范围响应仍需另行验证。固件的 Pump Fan 标签不代表物理水泵，本机该通道已由用户确认为 CaseOut 风扇。

若内核更新，需要重新构建和签名驱动，再运行服务安装器；沿用已登记的有效签名密钥时，无需再次登记同一证书。另一台电脑应从 [Ubuntu 安装指南](UBUNTU.md) 开始，不复制本机模块或通道绑定。

参考：

- [Linux 内核模块签名说明](https://docs.kernel.org/admin-guide/module-signing.html)
- [Ubuntu Secure Boot 文档](https://documentation.ubuntu.com/security/security-features/platform-protections/secure-boot/)
- [nct6687 驱动说明](https://github.com/Fred78290/nct6687d)
