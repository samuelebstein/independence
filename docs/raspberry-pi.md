# Raspberry Pi 5: first boot

This short guide owns only the new SD card, Raspberry Pi OS, Wi-Fi, and SSH.
It does not build Independence, install Hailo, download a model, or configure
audio/camera hardware.

## Overview

| Step | Done when |
| --- | --- |
| 1. Flash and boot | The Pi has a local monitor and keyboard attached |
| 2. Wi-Fi and SSH | SSH works over the home Wi-Fi, without Ethernet |
| 3. Confirm the OS | The Pi reports 64-bit Raspberry Pi OS Trixie |
| 4. Continue | Choose the Independence or Hailo guide below |

Start with a monitor and keyboard attached even if Raspberry Pi Imager was set
to enable Wi-Fi and SSH. That local console is the dependable way to recover
from an SSH or Wi-Fi setup failure.

## 1. Flash and boot

In Raspberry Pi Imager, write **Raspberry Pi OS (64-bit), Trixie** to a new SD
card. Set a username/password, enable SSH, and enter the home network's
separate **2.4 GHz** Wi-Fi SSID. This Pi is reliable on that 2.4 GHz network,
not the router's 5 GHz option.

Boot with a monitor and wired keyboard. Keep Ethernet connected until the SSH
test in the next section passes over Wi-Fi.

## 2. Connect Wi-Fi and enable SSH

This connects to the 2.4 GHz SSID without placing its password in shell
history, then shows the Pi's address.

```bash
nmcli radio wifi on
nmcli dev wifi list
sudo nmcli --ask dev wifi connect 'YOUR-2.4-GHZ-SSID'
nmcli device status
hostname -I
```

If the SSID is absent, run `sudo raspi-config`, choose `Localisation Options →
WLAN Country`, reboot, and repeat the block.

This enables the SSH service and proves that it is listening locally.

```bash
sudo systemctl enable --now ssh
systemctl is-enabled ssh
systemctl is-active ssh
sudo ss -ltnp | grep ':22'
```

The first two status commands must print `enabled` and `active`. From the
development computer, connect with the address printed by `hostname -I`:

```bash
ssh YOUR-PI-USER@PI-IP-ADDRESS
```

Only unplug Ethernet after that connection succeeds over Wi-Fi. If SSH says
`connection refused`, use the monitor and rerun the SSH block; no special file
is required.

## 3. Confirm the operating system

This runbook requires 64-bit Raspberry Pi OS Trixie. The two lines below must
remain together; the second line prints the values and is not a command to run
by itself.

```bash
. /etc/os-release
printf 'PRETTY_NAME=%s\nVERSION_CODENAME=%s\n' "$PRETTY_NAME" "$VERSION_CODENAME"
uname -m
```

Expect `VERSION_CODENAME=trixie` and `aarch64`. An Ubuntu/Noble result, an
empty codename, or a different architecture means reflash the SD card before
continuing.

## 4. Choose the next guide

- [Independence hardware setup](raspberry-pi-independence.md) installs the
  local application and proves the speaker, microphone, attached keyboard, and
  Camera Module 3.
- [HailoRT 5.3 + Qwen3-VL-2B-Instruct](raspberry-pi-hailo-qwen3.md) links the
  pinned AI HAT+ 2 system setup, verifies the accelerator, and proves one
  standalone Qwen3 image request. It does not yet make Qwen3 an Independence
  backend.
- [Qwen2-VL Hailo bridge](raspberry-pi-hailo.md) is the existing, working
  Independence Hailo path. It is a separate 5.1/Qwen2 setup; do not combine
  its Hailo installation commands with the Qwen3 guide on one SD card.
