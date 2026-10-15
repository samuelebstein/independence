# would you rather have me or openai have your data?

there is something i want. i want to be able to go out in the world without my phone, without an internet connection, and survive. i wrote more about this at the end of my first post on a vision i had a couple weekends ago. the tools we are using today are making us less capable as *human beings* than ever before. take away our ability to talk to the infrastructure of the digital world, and how does that make you feel? i also want to be able to ask any question, be as dumb as i want to be, ask about any health-related question or any example of something you might want to be private, and for that not to be looked at by any company. i (we) have a fundamental *distrust* of all of these big companies.

so i thought, maybe i should start with just me having the data and hosting the infrastructure myself. below, you'll find a guide to running ai inference in your own home. i can connect to this — albeit it's a little slow bc i don't have the right gpu right now — and know that all the queries, all the data, are stored locally in my home. nobody is looking at this. you can use it too. i gave friends the link to use it. i've redacted the domain name because i didn't guard it much with security (dm me if you want to try). what was so interesting was that they didn't feel the bit of magic that i felt. i felt *magic* when i was using my own locally hosted ai inference with open-source models and open-source infrastructure (openwebui, qwen models). i could ask it any question without any fear. have you ever worried about typing something into google that's a legal question or a health question, and wondered who can see it or who *might* see it in the future? as open-source models get better, the open-source infrastructure around them gets better, and edge devices continue to improve in their gpu capabilities, running my own locally hosted ai will become even more seamless. but, again, my friends didn't experience that *magic*. why? they wouldn't use this. they don't want *me* to see their queries. as the owner of the service, i can see everything. and when i'm talking to friends, they would rather have openai, meta, or anthropic see their information than me. there is anonymity in volume, and that makes them feel a little safer. but it pales in comparison to the safety you feel when that data is truly yours. if you run your own infrastructure and your own service, very few people are going to feel the magic that i felt after setting this up. what to do about it? i'm not exactly sure. just sharing my thoughts. and here is the instruction manual / documentation i wrote up for how i set this up at home. it's highly technical. many pieces have to fit together.

## how i set this up at home

### the computer

i used a separate linux computer on my home network. that is all i mean by a server here: a computer that stays on and runs the service. it does not have to be a rack of equipment. a spare computer with enough memory to run the model is enough to start. you can use your everyday computer too, but i think it is cooler to have another machine sitting there doing this.

that computer needs an internet connection for the outside access described below, and it needs to stay awake while people are using it. the model still runs locally; making the chat interface accessible over the internet does not move the inference to a cloud provider.

i already had docker, ollama, and `qwen3:8b` installed. the ollama setup (see [ollama.md](ollama.md)) has its own readme. the commands here are for my linux setup and use `apt` for installing caddy.

### the router

i use a turris router. it runs turris os, an open-source operating system based on openwrt, with web interfaces called reforis and luci for changing its settings. [1]

the router menus below are specific to turris. another router can work, but it needs to let me forward incoming connections to the server. an isp-supplied router will not necessarily give me the same administrative controls. for the setup below, the important capability is port forwarding; owning a turris is not a requirement for running open webui. [1, 2]

## 1. find the server and pull the image

first, on the computer that will be the server, open a terminal and run:

```bash
hostname
hostname -I
```

`hostname` prints the computer's name. `hostname -I` lists its network addresses. mine is named `mrfoxybrown`, and its address on my home network is `192.168.1.184`. if several addresses appear, use the one belonging to the connection to the home router, not a docker network. the router's connected-device list is another place to find that address. [3]

`samebstein` is my linux username. `mrfoxybrown` is the hostname i gave the server. neither is a special open webui setting. a terminal prompt like `samebstein@mrfoxybrown:~$` means i am logged in as `samebstein` on `mrfoxybrown`; the prompt itself is not part of a command.

from my regular computer, on the same home network, i connect to the server with ssh:

```bash
ssh samebstein@192.168.1.184
```

replace `samebstein` with your linux username and `192.168.1.184` with the address of your server. if you are already sitting at the server's terminal, there is no need to ssh into it. the docker and caddy commands below run on the server, not on the computer i am using to browse the chat interface.

i pulled the open webui image:

```bash
sudo docker pull ghcr.io/open-webui/open-webui:main
```

this is the open webui image without a bundled ollama installation, since i already had ollama running separately. [4]

## 2. create the application secret

i keep the application secret in a file so that starting a replacement container does not give the application a different secret.

these commands create it for the first installation:

```bash
mkdir -p "$HOME/.config/open-webui"
chmod 700 "$HOME/.config/open-webui"
openssl rand -hex 32 > "$HOME/.config/open-webui/secret-key"
chmod 600 "$HOME/.config/open-webui/secret-key"
```

`openssl rand -hex 32` generates the random value. the permissions restrict access to the directory and file to my user. this is an application secret, not the password i will use to log into the website.

i generate it once and keep it private. **do not rerun the generation command over an existing secret.** open webui uses `WEBUI_SECRET_KEY` for authentication and protection of some stored credentials; changing it can invalidate sessions and make those credentials unreadable. [5]

## 3. run open webui

```bash
sudo docker run -d \
  --name open-webui \
  --restart unless-stopped \
  --network=host \
  -e OLLAMA_BASE_URL=http://127.0.0.1:11434 \
  -e WEBUI_SECRET_KEY="$(cat "$HOME/.config/open-webui/secret-key")" \
  -v open-webui:/app/backend/data \
  ghcr.io/open-webui/open-webui:main
```

`--network=host` puts the container on the linux host's network. that lets open webui reach the ollama service at `127.0.0.1:11434`. open webui itself listens on port `8080` in this setup. [4, 6]

the `open-webui` volume stores the application data, including accounts and chat history, at `/app/backend/data` inside the container. docker creates the named volume when it is first used. keeping that volume is what keeps the data when the container is replaced. [4, 7]

`--restart unless-stopped` lets docker restart the container automatically unless i deliberately stop it. the secret is read from the file created in the previous step rather than generated again. [21]

## 4. open it on the home network

the container and its logs are visible with:

```bash
sudo docker ps --filter name=open-webui
sudo docker logs --tail 100 open-webui
```

from a browser on my home network, i open:

```text
http://192.168.1.184:8080
```

this is the address from step 1, with open webui's port added. substitute your server's local address. this first connection is plain http; caddy will handle public https later.

i created my administrator account before opening the service to the internet. the first account on a fresh open webui installation becomes the administrator. [8]

in **admin panel -> settings -> connections**, the ollama connection points to:

```text
http://127.0.0.1:11434
```

here, the connection is made by open webui on the server, so the loopback address is correct. `qwen3:8b` should be available in the model selector. [4, 6]

## 5. turn off thinking and keep the model loaded

the model was too slow. at this stage, i wanted to feel the magic of talking to something running in my own home. i did not really care about getting the most accurate output possible; i wanted it to respond without such a long wait. so i turned off its thinking mode and kept the model loaded between conversations.

i created a preset in **workspace -> models -> create**, using these settings:

| setting | value |
| --- | --- |
| name | `qwen3:8b no thinking keep alive` |
| base model / from | `qwen3:8b` |
| advanced parameters -> think (ollama) | off / `false` |
| advanced parameters -> keep alive | `-1` |

this is a saved configuration for the existing model, not a new model download or a model i trained. after saving it, i select that preset when starting a chat. [9]

`think=false` tells ollama to disable the model's explicit thinking mode. hiding or collapsing the thinking panel is not the same thing; the setting needs to be off in the model's parameters. [10]

`keep_alive=-1` asks ollama to leave the model in memory rather than unload it after the normal idle timeout. that avoids loading it again for the next conversation. it does not keep the model loaded through a restart, and it does not store chat history; open webui's data volume handles that. [11]

after using the preset, i checked:

```bash
ollama ps
```

the `qwen3:8b` entry showed `Forever` in its residency information. i put the settings on the shared preset rather than only in my personal chat controls, so other people could use the same configuration. [9, 10, 11]

## 6. let other people use it

when i started sharing the service, there were two separate problems: an account could be stuck on pending, and a user could lack access to the model.

### accounts

in **admin panel -> users**, change an existing account's role from **pending** to **user**. changing the default for new accounts does not approve accounts that are already pending. [8]

for new signups, the settings i wanted were:

| setting | value |
| --- | --- |
| signup | enabled |
| default user role | user |
| authentication / login | enabled |

these are changed in the admin settings. ordinary users do not need to be administrators. leaving signup open with a default role of user means anyone who reaches the site can create an account and use the available models; the link is not an invitation-only security boundary. [8]

### model access

i made **both** of these entries public in their model access settings:

| model | access |
| --- | --- |
| `qwen3:8b` | public |
| `qwen3:8b no thinking keep alive` | public |

open webui checks access to the preset and its underlying model. sharing only the preset is not enough. the user needs permission to use `qwen3:8b` too, or the request can fail with “model not found.” this is permission to use the models, not a reason to give other people permission to edit them. [9]

## 7. get a public domain name

i needed a name that could take a browser back to my home connection from outside my network. the local address `192.168.1.184` is not that address.

there are services that provide a public subdomain without requiring me to buy a whole domain:

| service | option |
| --- | --- |
| duckdns | a free public subdomain [12] |
| dynu | a free subdomain, or dns for a domain i already own [13] |
| no-ip | a free hostname option [14] |

using a subdomain of a domain i already own also works. the important part is being able to point the hostname at my connection and prove control of it, not which provider supplies the name. [22, 23]

in the provider's dns settings, the hostname needs an `A` record pointing to the router's **public, internet-facing ip address**, not the server's `192.168.1.184` address. the router's internet/wan status is where to look for its internet-facing address. the home connection also has to permit incoming traffic to reach the router. if the public address changes, the dns record needs to stay up to date. [13, 22]

### why the name matters for tls

a public hostname gives the browser a name to verify against a tls certificate. a subdomain can have a publicly trusted certificate just like a purchased domain can. i do not need to buy a separate certificate from the name provider: caddy can obtain one automatically. [15, 23]

https uses tls to encrypt the connection between the browser and my server and authenticate the hostname. that matters when sending passwords and private questions over the internet. it does not hide those questions from me as the server operator. [16, 23]

i have left my actual hostname out of this write-up. **`your-domain-name` is a placeholder everywhere below. replace it with your full public hostname before using the configuration or opening the address.**

## 8. install and configure caddy

i run caddy directly on the server, separate from the open webui container. it sits in front of open webui, handles https, and passes requests to the application. [16]

```text
browser --https:443--> home router --tcp:443--> caddy
                                                |
                                           http :8080
                                                v
                                            open webui
                                                |
                                           http :11434
                                                v
                                              ollama
```

caddy, open webui, and ollama are all on the same computer. the two http connections after caddy stay on that computer through `127.0.0.1`; the browser's connection across the internet is encrypted. [6, 16]

on the server:

```bash
sudo apt update
sudo apt install -y caddy
```

caddy has installation instructions for debian and ubuntu if it is not available from the package sources already configured on the computer. [17]

the configuration lives at `/etc/caddy/Caddyfile`. open that file:

```bash
sudo nano /etc/caddy/Caddyfile
```

for this site, the configuration is:

```caddyfile
your-domain-name {
    reverse_proxy 127.0.0.1:8080
}
```

replace `your-domain-name` with the full hostname from step 7. there is no `http://` prefix on the site name. `reverse_proxy` tells caddy to pass the requests to open webui on port `8080`. [15, 16]

save the file, then validate and reload it:

```bash
sudo caddy validate --config /etc/caddy/Caddyfile
sudo systemctl reload caddy
```

### how caddy gets a trusted certificate

putting the public hostname in the configuration tells caddy to manage https for it. caddy can request a certificate from let's encrypt using acme, the protocol for automating certificate issuance and renewal. [15, 23]

before issuing a certificate, let's encrypt has to verify control of the hostname. for the inbound challenges used here, its servers look up the hostname in public dns and connect to the resulting address. caddy answers a challenge to show that the service reached through that name is under its control. [18, 23]

with **http-01**, the check requests a special challenge response over port `80`. with **tls-alpn-01**, it uses a special tls handshake on port `443`. caddy handles the response; i do not have to create a page in open webui for it. this is why the dns record and router forwarding both have to be correct. [15, 18]

once validation succeeds, the certificate lets a browser check that it is connecting to the hostname i configured. caddy serves the certificate and handles renewal automatically. the certificate does not certify the model's answers or prove that the entire application is secure. [15, 23]

`caddy validate` checks the local configuration. it is not the same as let's encrypt reaching the server from the internet and validating the hostname. the next step makes that connection possible. [18, 19]

## 9. forward the router ports to caddy

on the turris router, the forwarding settings are under **reforis -> port forwarding**. the luci equivalent is **network -> firewall -> port forwards**. [1, 2]

the rules send incoming connections to the server:

| purpose | protocol | external port | server address | internal port |
| --- | --- | --- | --- | --- |
| http / certificate validation | tcp | `80` | `192.168.1.184` | `80` |
| https | tcp | `443` | `192.168.1.184` | `443` |

save and enable both rules. replace the server address with the one found in step 1. the destination address needs to remain the same; if it changes, the forwarding rules no longer point to the right place. turris documents a static dhcp lease as the prerequisite for this. [2]

the destination for port `443` is **caddy on port `443`**, not open webui on port `8080`. caddy handles tls first, then forwards the request internally. the public forwarding rules do not need to expose open webui's `8080` or ollama's `11434` directly. [15, 16]

## 10. check the server firewall

there is also a firewall on the server to consider, separate from the router. `ufw` is the “uncomplicated firewall” command-line tool. this shows whether it is active and which rules it is enforcing: [20]

```bash
sudo ufw status verbose
```

if ufw is active, these rules allow incoming tcp traffic to caddy's http and https ports:

```bash
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp
```

`allow` changes the server's firewall rules; it does not configure the router's port forwarding or turn an inactive firewall on. [20]

## 11. open it from outside the network

caddy's service status and certificate messages are available on the server with:

```bash
sudo systemctl status caddy --no-pager
sudo journalctl -u caddy -n 100 --no-pager
```

when the certificate was issued, the logs included:

```text
certificate obtained successfully
```

the public address is then:

```text
https://your-domain-name
```

there is no `:8080` on this address. the browser connects to caddy over https, and caddy passes the request to open webui. [16, 22]

this is the kind of link i sent to my friends. they still need an open webui account, and the account and model permissions from step 6 still matter. the model and the saved conversations remain on the computer in my home.

## references

| ref | source |
| --- | --- |
| 1 | [turris os and its administration interfaces](https://docs.turris.cz/basics/os-faq/) |
| 2 | [turris port forwarding](https://docs.turris.cz/basics/reforis/port-forwarding/port-forwarding/) |
| 3 | [linux hostname command](https://manpages.debian.org/bookworm/hostname/hostname.1.en.html) |
| 4 | [open webui quick start](https://docs.openwebui.com/getting-started/quick-start/) |
| 5 | [open webui faq: application secret](https://docs.openwebui.com/faq/) |
| 6 | [docker host networking](https://docs.docker.com/engine/network/drivers/host/) |
| 7 | [docker volumes](https://docs.docker.com/engine/storage/volumes/) |
| 8 | [open webui account roles](https://docs.openwebui.com/features/authentication-access/rbac/roles/) |
| 9 | [open webui model presets and access](https://docs.openwebui.com/features/workspace/models/) |
| 10 | [open webui thinking-model settings](https://docs.openwebui.com/features/chat-conversations/chat-features/reasoning-models/) |
| 11 | [ollama faq: keeping a model loaded](https://docs.ollama.com/faq) |
| 12 | [duckdns](https://www.duckdns.org/) |
| 13 | [dynu](https://www.dynu.com/en-US/DynamicDNS) |
| 14 | [no-ip](https://www.noip.com/free) |
| 15 | [caddy automatic https](https://caddyserver.com/docs/automatic-https) |
| 16 | [caddy reverse proxy](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy) |
| 17 | [caddy installation](https://caddyserver.com/docs/install) |
| 18 | [let's encrypt hostname-validation challenges](https://letsencrypt.org/docs/challenge-types/) |
| 19 | [caddy command-line tools](https://caddyserver.com/docs/command-line) |
| 20 | [ubuntu firewall documentation](https://ubuntu.com/server/docs/how-to/security/firewalls/) |
| 21 | [docker restart policies](https://docs.docker.com/engine/containers/start-containers-automatically/) |
| 22 | [caddy https quick-start](https://caddyserver.com/docs/quick-starts/https) |
| 23 | [how let's encrypt validates domains and issues certificates](https://letsencrypt.org/how-it-works/) |
