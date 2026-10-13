# how i configured my gpu on my system76 meerkat device

## identify the gpu and its driver 

```bash
lspci -nnk | grep -A3 -Ei 'vga|3d|display'
lspci | grep -i 'vga\|3d\|2d'
```

### what this does

`lspci` lists the pci devices attached to the comptuer, including graphics controllers, network adapters, and storage controllers

### why i used this 
i wanted to identify the gpu on the device and determine if linux loaded its driver.

the server reported:

```bash
Intel Corporation Raptor Lake-P [Iris Xe Graphics]
Kernel driver in use: i915
```

which means that linux recognized the gpu and loaded intels' i915 graphics driver (a lot of this i don't understand... need to dig in more)

## check ollama is running

```bash
ollama run --help
ollama run qwen3:8b
ollama ps
```

### why i did this 

`ollama ps` shows me if ollama is using cpu or gpu for inference. i got:

```bash
NAME        SIZE      PROCESSOR    CONTEXT
qwen3:8b    5.9 GB    100% CPU     4096
```

## inspect logs 

```bash
sudo journalctl -u ollama -b --no-pager -n 100

sudo journalctl -u ollama -b --no-pager \
  | grep -Ei 'vulkan|gpu|library|inference' \
  | tail -n 40
```

`journalctl` reads logs from `systemd`'s journal


### why i did this
ollama's installer reportered no nvidia or amd gpu (because i have an intel gpu)

in the logs:

```bash
OLLAMA_VULKAN:true
OLLAMA_IGPU_ENABLE:
```

`vulkan` support was enabled, but ollama wasn't detecting an eligible gpu. the `OLLAMA_IGPU_ENABLE` flag was unset 


## install vulkan support 

```bash
sudo apt install -y mesa-vulkan-drivers libvulkan1 vulkan-tools
```

## enable integrated gpu support in ollama

```bash
sudo systemctl edit ollama

cat /etc/systemd/system/ollama.service.d/override.conf
```

### why i did this

`systemctl edit ollama` opens an idtor to create or modify a systemd configuation. i added:

```bash
[Service]
Environment="OLLAMA_IGPU_ENABLE=1"
```


## applying the changes

```bash
sudo systemctl daemon-reload
sudo systemctl restart ollama
```

## verify gpu acceleration

```bash
ollama run qwen3:8b
ollama ps
```

and the output!

```bash
NAME        SIZE       PROCESSOR    CONTEXT
qwen3:8b    10.0 GB    100% GPU     32768
```
