# the offline revolution

just as solar allows you to bypass electric grids and cell phones bypass telephone lines, offline compute will analogously bypass the internet. the first allows you to get energy straight from the sun, the second gets you connection straight from a cell tower and the third gives you compute and knowledge right from the device you're holding. 


the beginnings of the internet were beautiful. colleges creating hteir own servers, communication through electricity allows near instance communication across the globe. but web 2 infinltrated like a something on something something (toxic blah blhah blah). today 40% of traffic goes through cloudflare. that means 40% of all requests someone makes to a software service (amazon, blah blah, netflix), all of the information at our fingertips has a few single point of failure. ther's been a lot of talk about new ai models and how they're muc hbetter at finding bugs and exploiting them. thats true, and the reality is they only have to find one or two pathways to disrupt most of the communication in the world .

here's a visualization of how ai models combine small vectors of attack to achieve something magnificent and deadly. imagine you're a cyberattacker. there are pretty much two things you're trying to do. you're trying to take down the service or you're trying to steal something.

it is posisble to take down a service from the outside. engineers call this a dos attack or ddos attack. dos stand for "denial of service". generally an attack liek this consists of the attacker combining many many different servers (chips) and using those servers to all point at the exact same target. the target might be an "API". for example, calling an API looks like this:

curl --post "google.com" --route /api/getmail --parameters x,y,z" sometimes when you "hit" the same service with a combination of characters. so the parameter x is a string like "" those parameters can be manipulated to take down the server. for example if one of the parameters is really long, ie it doesn't have a length limit. its supposed to be like paremeter(email=sam.ebstein@gmail.com) but if you send it parameter(email=asd;flkajincealieninc/alkdjfaslkdfja/s.f.3mbxxuain3...) and its a really big string bc there are no restrictions on length, the server that ultimately tries to process this might fail due to memory issues. now that one request hits just one server but if you run that request a million times at the same api and with similar parameters, you can take down ever server that company runs to a point where they can't server customer requests because there servers are cycling up and down due to memory constraints. 

those are the ones you see the most in the wild. lets take down this company so we can ransom/blackmail them into paying us to stop what were doing. while this is happenign, the company is trying to fix the issue. in the example above, they would be trying to find the api the attacker is hitting and figure out that the parameter doesn't have a length restriction. then they'll fix it, and deploy to prod. btu that might take hours. so htey will pay the attacker. 

this is ... relatively easy to do as an attacker.

there are ways to steal information from companies without getting inside of their network (injection techniquies. maybe a company doesn't make sure ceratin characters wil lexpolt. there was a massive issue with this with the log4j attack a few years ago. this is harder and less frequent.

the second, much harder and much more lucrative cyber attack is to steal information or take down the service from the inside.

getting inside is much harder than attack from outside. the first scenario was akin to throwing a fishing pole into a lake and hoping you get a fish. the second is like jumping into a lake where you cant miss if you have knife. there are so many fish inside the lake, you're just trying to figure out what you want to catch. getting in is extremely hard but causing a fucking shitstorm while you're in and getting out are both pretty easy. . but while you're in there is like jumping into the lake with a grenade 

the protections _within_ a company are much worse than the protection around. its similar to a massive wall around a castle. it might be hard to get over but once you're inside you just have to convince the right person to let you walk around 

at teh same time as this is happening, chips are getting better and better and we are reaching 
