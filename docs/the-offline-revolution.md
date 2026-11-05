# the offline revolution

the internet began as a beautiful movement of counterculture during the 1960s that reframed computers from centralized governmental power to tools of collaboration and personal empowerment. its not a coincidence that much of it was built at the same time and in the same place as the central point of psychedelic experimentation and rejction of traditional instittuions. the creators of the internet were trying to democratize access to information. 

fast foreard to the present day and our source of information and intelligence is practically three companies. and the internet, originally held up by colleges and univerisites around the world has itself become centralized. 40% of all internet traffic goes through cloudflare and nearly 2/3 of all data center cpacity is hyperscalers with the biggest shares amazon, google and microsoft. tldr, we've lost most of what made the internet great. 

why does this matter?

just as solar allows you to bypass electric grids and cell phones bypass telephone lines, offline compute and intelligence will analogously bypass the internet. 

every piece of hardware and software you use has the internet baked into it. the iphone is a portal to the infrastructure of the internet; whehter its your photos saved in the cloud, instagram hosted on meta server, email hosted by google, chatgpt hosted by microsoft, or pretty much every other service hosted by amazon., your traffic makes its way to one of these companies. and every server run by these companies runs much of that same code that originated in the stoned, and tripping minds of those fun bay area people.

this was mostly a tangent and fun to think about. but its important to acknowledge because when you look at the low level linux code, a lot of doesn't make any sense at all. its a maze and it often doesn't follow rules. just as many 


here's a visualization of how ai models combine small vectors of attack to achieve something magnificent and deadly. imagine you're a cyberattacker. there are pretty much two things you're trying to do. you're trying to take down the service or you're trying to steal something.

it is posisble to take down a service from the outside. engineers call this a dos attack or ddos attack. dos stand for "denial of service". generally an attack liek this consists of the attacker combining many many different servers (chips) and using those servers to all point at the exact same target. the target might be an "API". for example, calling an API looks like this:

curl --post "google.com" --route /api/getmail --parameters x,y,z" sometimes when you "hit" the same service with a combination of characters. so the parameter x is a string like "" those parameters can be manipulated to take down the server. for example if one of the parameters is really long, ie it doesn't have a length limit. its supposed to be like paremeter(email=sam.ebstein@gmail.com) but if you send it parameter(email=asd;flkajincealieninc/alkdjfaslkdfja/s.f.3mbxxuain3...) and its a really big string bc there are no restrictions on length, the server that ultimately tries to process this might fail due to memory issues. now that one request hits just one server but if you run that request a million times at the same api and with similar parameters, you can take down ever server that company runs to a point where they can't server customer requests because there servers are cycling up and down due to memory constraints. 

those are the ones you see the most in the wild. lets take down this company so we can ransom/blackmail them into paying us to stop what were doing. while this is happenign, the company is trying to fix the issue. in the example above, they would be trying to find the api the attacker is hitting and figure out that the parameter doesn't have a length restriction. then they'll fix it, and deploy to prod. btu that might take hours. so htey will pay the attacker. 

this is ... relatively easy to do as an attacker.

there are ways to steal information from companies without getting inside of their network (injection techniquies. maybe a company doesn't make sure ceratin characters wil lexpolt. there was a massive issue with this with the log4j attack a few years ago. this is harder and less frequent.

the second, much harder and much more lucrative cyber attack is to steal information or take down the service from the inside.

getting inside is much harder than attack from outside. the first scenario was akin to throwing a fishing pole into a lake and hoping you get a fish. the second is like jumping into a lake where you cant miss if you have knife. there are so many fish inside the lake, you're just trying to figure out what you want to catch. getting in is extremely hard but causing a fucking shitstorm while you're in and getting out are both pretty easy. . but while you're in there you ahve a time limit. ghow long until someone figures out what you're doing and once you start being obviously malicious, that countdown starts. is like jumping into the lake with a grenade 

the protections _within_ a company are much worse than the protection around. its similar to a massive wall around a castle. it might be hard to get over but once you're inside you just have to convince the doorman wiht a $30 to eat at the huge feast. the right person to let you walk around .


ai models are much better at getting _in_ than humans. the reason is they're so good trying every possibel combination and quickly. and once they're _in_, they can wreak all kinds of havic. think about your vibe coded project. if you're not specific enough about what you meant, you're never going to understand the changes it made. and even if you think you did a good job prompting it, it will wiggle free and do somethign ridiculous! "nooooo, i literally said don't do that!". - how many times have you said that to your ai.


the reason they're so much better is an llm is harnessing electiity and the power of iteration towards human language and all code is written in human language. just as chess is a game that can be won and lost, code is a game that can be won and lost. there are twists and turns and techniques, attacks and defenese at aevery level. which is why you hear that that anthropic is giving their latest model to all businesses for free. its in the their best interest to shore up the holes in each and every business so that the internet doesn't break, people don't lose confidence in the internet, and every business is now a customer of them. but in my opinion its a losing battle. the deeper you go into low level linux code, the more you realize nobody understands it. there are rules and abstractions, but just like conjugations in human language don't always follow the rules, netiher does code. its brittle in some places, confusing in others. it was written by people in the time of lsd and smoking weed. the people who rote it we're using their new neural pathways for . its an amalgamation of so many people contributing to make something work. and the same low level linux libraries are on every server on the intenret. btu if you iterate and yo uiterate fast neoguh, there's not much of a limit


i keep thinking of anthropic, openai, google, apple and meta as what star wars calls the empire. and they're building the death star: a technological marvel capable of complete and utter destruction
 . all the money and computation th could be used to solve he worlds probelsm they are using to create super intelligence. but for what? they haven't really specified that. a future where all food is super abundant, a future where there is universal healthcare and monthly checks? 

look up how much they're spending on datacenters, ai infrastructure and compare that against health care costs, number of children in the coutnry that aren't hitting minimal nutritional numbers. 

they could do those things right now. but they're not. they're not going to do it when they're more powerful and rich.

but as they're building up their powoer and their superintelligent systems, and getting us more addicted to renting their intelligence than cocaine, they're forgetting that what they're building will also be there downfall. their profit, their businesses depend on the internet existing and people, businesses, machines having access to their technology. the technology they're building will break everyone's access to it. its just a matter of time. its their achilles heal and another reason why they're not making their more advanced models public. bc they need to train the models to not attack themselvves! some super intelligence. 

its weird how this pattern is understood by so many cultures but less so ours.

once that happens, communities and people will have to build themselves up from the ground up. supply chains will be destroyed, not having an ability to communicate with the speed and efficiency of the itnernet, they will fall apart. and we'll be left in a place where offline technology will ehlp everyone learn the basic things we've forgotten over time. how to grow our own food, how to make our own shelter, how to talk to our neighbors and play outside. 

compute and knowledge is getting cheaper and cheaper, and more easily run on small devices. you combine that with cheaper solar energy and you get unlimited, free knowledge right from the source. and you get it with privacy, without the empire, and it will feel good, natural, right. 

i'm trying to prepare myself for that, because i do believe its coming. but if it doesn't, i'd rather work towards a future that doesn't need those companies anyways. 





at teh same time as this is happening, chips are getting better and better and we are reaching 
