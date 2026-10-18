# 在另一台 Ubuntu 电脑安装

FanControlLX 的应用代码可以复用；风扇驱动、通道绑定和签名证书按每台机器单独接入。当前版本的后台安装器针对本机 NCT6687 驱动，其参数不是任意主板的通用安装方案。

## 获取应用并检测硬件

首次发布到 GitHub 后，在目标电脑执行：

```bash
sudo apt update
sudo apt install git python3
git clone https://github.com/Skywalker144/FanControlLX.git
cd FanControlLX
python3 --version
./webui.sh --diagnose
```

需要 Python 3.10 或更新版本，不需要 Conda、pip、npm 或桌面环境。诊断命令只读取硬件信息。保留完整结果，尤其是 `board`、`kernel`、`chips`、`channels` 和 `gpu_error`，据此选择驱动。普通用户下出现不可写权限提示，不代表驱动不工作。

```bash
./webui.sh
```

在目标电脑打开 <http://127.0.0.1:8090>。如果通过 SSH 远程使用，在浏览器所在电脑建立转发：

```bash
ssh -N -L 8090:127.0.0.1:8090 用户名@目标电脑地址
```

这个首次启动的前台进程用于监控和编辑，尚未安装后台服务，关闭终端会退出。想先看界面可用 `./webui.sh --demo --port 8091`，演示数据与真实配置分开。

## 接入目标电脑的风扇

先根据诊断结果判断：

| 诊断结果 | 下一步 |
| --- | --- |
| 已有受支持驱动和 PWM 通道，仅权限不足 | 停止前台监控，再用 `sudo ./webui.sh --foreground --control` 识别设备 |
| 没有 PWM 通道 | 根据主板与芯片选择并加载对应驱动，重新诊断 |
| 驱动提示尚未验证、仅监控 | 需要验证该驱动的控制和恢复接口，再扩展程序支持 |
| GPU 温度不可读 | 检查目标电脑显卡驱动；当前自动 GPU 温度读取使用 `nvidia-smi` |

不要因为本机使用 NCT6687，就给另一块主板安装相同驱动或启用 `msi_fan_brute_force=1`。Linux 自带且受信任的驱动通常不需要自行签名；只有需要第三方模块且启用 Secure Boot 等验证机制时，才需要按目标系统的要求签名并登记证书。驱动必须匹配目标电脑正在运行的内核。

有可用通道后，逐个使用 100% 测试与滑块确认实际位置，确认风扇/水泵类型并命名，再设置温度源和曲线。多个风扇接在同一分线器上，通常只能作为一路控制。对没有已验证测速映射的驱动，需要手动配对 PWM 与 RPM。

不要直接复制第一台电脑的 `data/config.json`。其中包含设备 ID、通道、身份确认及自动控制策略。需要复用曲线时，在目标电脑重新识别并绑定设备后，录入相同的曲线节点与参数。

## 安装后台服务

若目标机器也使用已经验证兼容的 NCT6687 驱动，需要在目标机器针对其当前内核构建模块，准备好签名文件 `.runtime/nct6687d/nct6687.signed.ko`，并完成该机器需要的证书登记。先通过前台控制确认设备，保存配置并结束前台进程，然后执行：

```bash
./install-service.sh --dry-run --enable-confirmed
sudo ./install-service.sh --enable-confirmed
systemctl is-active fancontrollx
systemctl is-enabled fancontrollx
./webui.sh
```

前两条状态检查应分别返回 `active` 和 `enabled`。再在网页确认每路均正在控制、温度与转速正常。具体后台行为见 [BACKGROUND.md](BACKGROUND.md)。当前安装器要求已签名模块，即使目标系统没有启用 Secure Boot 也会进行这一检查。

如果目标电脑使用其他芯片，应用可以检测其硬件，但本版安装器和服务模板仍固定加载 `nct6687`。需要根据该电脑实际驱动适配后台安装，不能直接执行上述安装步骤。

签名私钥、内核模块、真实配置和运行数据不包含在 Git 仓库中。不要把第一台电脑的签名私钥当作应用文件搬过去；第三方驱动在目标电脑单独构建并处理信任。

## 更新应用

应用已经安装为后台服务后，在项目目录执行：

```bash
git pull --ff-only
sudo systemctl restart fancontrollx
systemctl status fancontrollx --no-pager
```

配置保存在被 Git 忽略的 `data/` 中。修改服务模板或重新构建驱动后，需要再运行安装器；普通代码更新重启服务即可。内核升级后必须重新编译、签名、安装第三方模块，当前没有 DKMS 自动重建。
