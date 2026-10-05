# independence

i’ve had a few conversations with friends that seem, at first, to be about different things. we talk about, work, climate change, artificial intelligence, art, and where we want to live. but underneath those conversations, i keep hearing versions of the same question: how do we build a life we believe in, in a world we don’t feel particularly in control of?

some of us want financial independence. some want work that feels worthwhile. some want to live closer to friends and spend more time with the people they love. there is excitement about what technology might make possible, but also confusion about what it is doing to us. will our skills still matter? are the tools we use making us more capable, or are we becoming less capable without them?


## inspiration 

talking with b, i realized how poorly prepared i feel for a world even modestly different from the current one. i have spent the last seven years teaching myself computer science. those skills are valuable now. they have allowed me to work and support myself. i’m less sure what they will mean in ten years, or how useful i would be outside the particular arrangements that make them valuable today.

b has a much more diverse collection of practical skills and knowledge. there is something reassuring about that. being able to take care of yourself gives you a foundation. it makes taking a risk feel less like stepping into nothing.

i know something about the systems that keep computers running. i know much less about the things that keep me alive: shelter, food, water, hygiene. i struggle to cook, navigate, and, often, communicate. i don’t think a survival course would go particularly well for me.

part of this worry takes an extreme form. what happens if the world changes quickly? will the place i live remain livable? what would i do if the systems i rely on stopped working? but underneath the disaster scenario is a much more ordinary desire: i want to understand the world around me, and i want to feel capable of participating in it.

my conversations with c approach this from another direction. he is an artist, and the shape of a normal working life in america does not feel natural or good to him. he is surviving, but surviving is not the whole question. how do you make a life inside a world whose expectations don’t fit you? how do you help make a world you actually believe in?

d thinks about financial security, starting a company, and finding an idea worth taking a risk on. but the freedom he wants has a particular shape: time with friends, proximity to the people he cares about, more control over his life. he is also interested in private inference, using models without surrendering the privacy of the information you bring to them.

e approaches these questions through open-source software. he refuses to work on proprietary code and is interested in building a more provable internet. his work with linux and trusted execution environments is tied to a larger ambition: a more equal, democratic, benevolent society. what interests me is that the technical choices are not separate from that ambition. they are one way of trying to realize it.

these are not identical concerns, but i recognize myself in all of them. i want a stronger foundation. i want work and tools that fit the kind of life i want. i want to be close to people. i want to understand more of what i depend on.

## the things i am using seem to be making me less capable 

this is part of why my relationship with ai feels conflicted.

i want to learn, and i keep turning to frontier models to help me. at the same time, i dislike the arrangement. i am paying a company to mediate my access to knowledge, and i am handing it my questions in the process. i don’t feel comfortable not knowing what becomes of those questions, what they reveal about me, or how that information might eventually be used.

i also resent the feeling that knowledge people have created and shared is being gathered up and sold back to them. e described open models to me as a way for the community to take that knowledge back. i find that idea compelling: not just getting a cheaper answer, but possessing the means to produce one.

i have a strong dislike of apple products, probably encouraged by spending time with people who dislike them much more than i do. but i’m trying to get more precise about what bothers me. it isn’t that the devices lack capability. it is that so much of the experience hides where that capability lives, how it works, and who controls it. you feel tethered to the thing because the more we use it, the less capable we become.

the iphone brought computing to our fingertips. but much of what i do with it makes it feel less like a computer i possess and more like a portal to infrastructure elsewhere.

spotify is a an example. i have an app on my phone, but the app is my interface to a service and its catalog. i possess access to spotify. i don’t possess spotify. i see a similar arrangement when i use gmail, instagram, or a cloud-based model: the thing in my hand is how i reach the thing i need.

of course, not everything on a phone works this way. i can write notes or play locally stored music without a connection. the distinction is not that an iphone is useless offline. it is a question of where the capabilities i care about live, and what conditions i have to satisfy to keep using them.

what would happen if i started from the other direction?

instead of beginning with a device that connects me to services, i could begin with a device that contains the capabilities i want. the intelligence would run locally. the knowledge and maps would be stored locally. my data and keys would stay with me. the software would be open. communication with another device would not necessarily require both of us to pass through someone else’s service.

the network could still be useful. but it would be something the computer could use, rather than the place where the computer’s purpose resides.

## a library and librarian that can become more knowledgable and accessible without any corporation

the part of this i love most is local knowledge first.

i don’t want to start with an empty chat box connected to a model somewhere else. i want to start with a library i can browse: encyclopedias, textbooks, repair manuals, maps, reference books, and my own documents. i want the material itself to be there, on storage i control.

the model would sit on top of that library. it would be a librarian, not a replacement for the books.

i could ask how a diesel engine works, and it could help me find and understand an explanation. i could ask it to teach me calculus. i could use a camera to ask questions about something in front of me, or use local speech recognition and translation to help understand another person.

the important part is not just that an answer appears. it is that i can get to the material behind the answer. i can follow a reference, inspect a diagram, or decide to read the chapter myself. the interface should make the knowledge easier to approach, not put another opaque layer between me and it.

that is a different relationship from asking a distant system a question and accepting whatever it sends back. the library is here. the program helping me explore it is here. i can use one without having to surrender the other.

i also don’t want to confuse carrying knowledge with having learned it. a device full of manuals would not suddenly make me practically competent. what i want is something that helps me build that competence: an explanation when i am curious, a reference when i am stuck, a way into a subject i don’t yet understand.

but then why isn't this just an app?

a local model could be an app. a library could be an app. maps could be an app. apple could build many of these capabilities, and they can make infinitely better hardware than i can.

but i am interested in making this the starting assumption of the whole device, rather than one optional feature inside it. i want to choose the model, inspect the software, control the storage, replace the battery, and learn how the thing works. i want the library to remain usable whether or not i use the model. i don’t want continued access to the essential capabilities to depend on an account or a company’s willingness to keep providing a service.

a useful test would be what happens if the company that made it disappears. the device should still perform its core functions. its knowledge should still be readable. its software should still be runnable. it should remain something i have, not something i used to be allowed to access.

## building local knowledge from the ground up

there is another part of the idea that makes it more than a private library.

suppose i encounter someone with another device. they have a newer map, or a collection of repair manuals i don’t have. why shouldn’t they be able to share it directly with me?

i imagine signed, versioned knowledge packs moving between devices. someone publishes a set of reference materials. other people keep copies and help distribute them. a person with the 1998 bmw repair manuals can pass them to someone who needs them. knowledge moves from hand to hand, rather than always making a round trip through a central service.

i imagine something with bittorrent's distributed ownership of information and airdoprs ability to move information directly between nearby devices.

communication could follow a similar principle. e introduced me to meshtastic and lora, where a device can also participate in a network. if mine can reach yours, and yours can reach someone else’s, perhaps a message can travel beyond the people i can reach directly.

i can imagine devices around los angeles forming connections that do not require a cellular service for every exchange. under the right conditions, another person joining would make the network more useful to the people already there. the device would be useful by itself, but it could gain something from being among others.

further out, there is the possibility of shared observations. devices might have cameras, location information, air-quality sensors, or other modules. people could choose to contribute observations about smoke, a tremor, or a wildfire. asking what is happening nearby could mean learning from the people and instruments nearby, not only consulting a distant platform.

i like the possibility of tools that are useful on their own and become more useful when people bring them together.

## i just want to learn 

i am interested in building and using the thing. that seems like a reasonable place to start. i haven't run a local llm. i've never really put together any pieces of hardware. 

i want to build something i can hold, disconnect it from the internet, and take it outside. i want to see what it helps me notice, understand, and do. i like the idea of beginning ten blocks from home, then a hundred, then a thousand. how far could i go? what would i need to know? what would i discover i had forgotten to put in the library?

there is a survivalist impulse in that, but there is also curiosity. i don’t need a disaster to want to understand a machine, learn about a plant, find my way somewhere, or talk to another person. i want the device to have a reason to exist on an ordinary day.

i don't know if there are companies building something like this but each individual piece exists already and it would require me to learn A LOT. also, i think my dad would find it cool. could i make it at home? maybe with a 3d printer i can build an enclosure for all the parts? could it have some sort of solar charging?

an iphone is designed primarily as a portal to services and infrastructure elsewhere; this device is designed around the opposite assumption: the intelligence, knowledge, software, data, and essentail capabilites should live with and belong to the person holding it. it should work without the internet, communicate directly with other devices, be open and repairable, and remain fully useful even if the company that made it disappears. 
