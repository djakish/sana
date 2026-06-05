# Faster vector search

November 13, 2025•The Database School Podcast

[Video 3](https://www.youtube.com/watch?v=K3ivDkMEclg)

## Transcript

**Aaron Francis** [0:00]:

Hey y'all, welcome back to the show. This is Database School. I'm your host Aaron Francis and today I have Simon Eskildsen on. He is the co-founder and CEO of a very cool company called turbopuffer. And turbopuffer is a serverless vector search, full-text search, interestingly built on top of object storage. It's built on top of Amazon S3 and, as we get into in the interview, also on Google Cloud and Azure Blob Storage. The interesting thing about this is it makes it obviously extremely scalable, but also way, way, way cheaper. He talks about some of the numbers in this episode, the number of vectors they're doing. It is insane. And weirdly, their first customer was a little company you might have heard of called Cursor. I don't know how that's their first customer, but this is a great show. He's very, very technical. I think you're very much going to enjoy this one. Please subscribe to the YouTube channel or visit the website at databaseschool.com and let me know what you think. Enough from me, let's get to the show with Simon. I am here today with the co-founder and CEO of turbopuffer, his name is Simon Eskildsen. We're going to talk about a lot of stuff. We're going to talk about scaling. We're going to talk about founding companies. But Simon, do you want to introduce yourself a little bit further?

**Simon Eskildsen** [1:29]:

For sure. Yeah. I mean, you did a good job. Today, I spend most of my time working on a search engine called turbopuffer and building the company around it. I came of age spending almost, you know, just shy of a decade working on infrastructure at Shopify, pretty much anything to make sure that the site stayed up and stayed scaling for all of the big customers and the tremendous growth of that company through the 2010s. And then after that, I helped some friends with infrastructure challenges at their companies, kind of in little three-month stints. I called it Angel Engineering. I took equity for improving Postgres query plans and optimizing AutoVacuum for a few years. And that's where I discovered the problem that eventually led to turbopuffer: that the incumbent search engines were not a delight, which was exactly my experience at Shopify. Now I've run turbopuffer for about two and a half years alongside a fantastic team here, and we're very proud to power many of the companies at the frontier of AI.

**Aaron Francis** [2:36]:

Wow, that was tight. That was really well done. I want to talk about what felt like the glory days for Rails engineers—the era you're describing: early 2010s, Heroku, GitHub, Shopify, everybody on Rails, and you all were the cool kids. I was so jealous of that whole scene.

**Simon Eskildsen** [3:00]:

What do you mean—like, whether I was living that Rails story, or something else?

**Aaron Francis** [3:04]:

Right, sorry—I'll set my side first so you know where I'm coming from. I wasn't in the Rails or San Francisco crowd; I was in PHP. I love PHP, but I wasn't one of the cool kids. I graduated in 2011 with a master's in accounting, spent a year at Ernst & Young as a tax accountant, and _that_ stretch is where I felt totally lost—the work wasn't for me. I'm still glad Ernst & Young gave me that year; I just mean I was off in a different world from the Rails scene. I missed the wave of Rails companies that became billion-dollar businesses. We didn't really have that parallel story in PHP land.

So I'm curious about your path: you were at Shopify for, what, eight years, scaling from something like a thousand requests a second to, call it, a million—at least the right order of magnitude. What was that like? What did you come in as—just a Rails developer? What did you end up as? What was the journey through that hypergrowth?

**Simon Eskildsen** [4:03]:

That's exactly right. I started, I mean, I... I started in PHP too. It's so good. The feedback loop is so good. I still miss it, you know, creating websites for the World of Warcraft guild. And then at some point, you're writing the same thing a million times. You build your own framework—that's how it goes. I probably did that too: how to build your own PHP framework from scratch. And then this dude in my World of Warcraft guild was like, "Have you looked at Ruby on Rails?" And I'm like, you go to the website back then, and I was like, "Is this like a toy or is like what is this?" and was very intrigued. And then got into Rails and I spent a lot of time with Rails in high school, worked for a company in high school, a startup in high school doing Rails. And then Shopify found me because I wrote this article about dropping my iPhone and it just dying. You know, they're like the 2010, 2009 iPhone. You dropped it once. Actually, this is funny. I think about it. I dropped my iPhone this morning and it didn't get destroyed. But back in 2009, if you dropped an iPhone once, it was dead, right? Screen smashed, it was game over. So you guarded that thing like an absolute... like, you know...

**Aaron Francis** [5:23]:

OtterBox cases were a thing. I remember that.

**Simon Eskildsen** [5:27]:

...and I didn't have one of those, and I just dropped it, and it got destroyed. And then I wrote an article about how I suddenly had my sense of direction back because I wasn't looking at a map all the time. It was like I had an attention span and just sat and looked into the ether, and it was actually pleasant. This was before we had realized all these pernicious effects of smartphones. It was just not a thing in 2010. And this article blew up, or sort of 2013, this article blew up, and Shopify reached out. I don't think they realized that I was in high school until I told them that the start date had to be after the semester. I was like, "Aren't you like..." Anyway, so I started there in 2013 as a Rails engineer, and I sat down to lunch one day, and I was so intrigued by what the infrastructure people were talking about. It was this like load balancers and, you know, squid here and varnish there and like Nginx. And, you know, I don't know, they probably used them all together, even though that was crazy. And I didn't understand like what's a proxy, what's a reverse proxy. I don't think I still know what a reverse proxy is. I don't know if anyone knows why it's called that. But this was very intriguing to me, and I just started picking up stuff that was in their issues and just started doing stuff with that team. And then somehow, like, found myself there working on Docker. And, you know, 2013 was a crazy time to get into Docker and got into that, got Shopify running in containers, and that was sort of my foray into infrastructure from Rails.

**Aaron Francis** [7:07]:

Yeah, it's funny that you talk about like in 2013 you dropped your phone and the world opened up before you. It's like how quaint. Like, dude, can you imagine like in 2013 looking forward to 2025 and just like realizing how much we're all going to be on our phones? And you had that realization a decade ago? Like, man. And another thing I want to highlight is something I talk about quite often is like, you really, one, not you specifically, one really needs to be writing and putting their work out there. And even if that's, you know, tweeting or making videos or posting on LinkedIn, God forbid. And like your story is very similar to how I got my first PHP job, which was I was doing something and I wrote about it on my blog, and some company reached out and was like, "Hey, we're doing that too. Can you come work for us?" And we'll pay you what to me was like a huge amount of money. And I was like, "The internet is amazing." And so it's nice to hear stories where you put something out there and good things come your direction. That's really encouraging for me.

**Simon Eskildsen** [8:10]:

I feel like another way to describe my teenage years was as a series of blog posts that made it to Hacker News, even if it was for a brief period of time. That's how I got my first job. It was how I went to Shopify. It was how I got my second job. Well, actually, the job I had in high school was because allegedly one of the co-founders was looking. I grew up in a small town in Denmark, and they were looking for... I should do this. I need to take a note of that. They were looking for people who were committing on GitHub. You know, again, just committing to GitHub in 2010, super strong signal. And committing to GitHub in 2010, 2009, between the hours of 2 a.m. and 4 a.m. in this small town. And they somehow found me through that. That was their talent sourcing process. I need to do that. You go to, you find, you go to like some tiny town in Nebraska and you just zoom in and you just geofilter into that, and you just find Alex, and Alex is just up, and it's 3 a.m., and she's just slinging C code because she still hasn't heard of Rust because no one in the town has even heard of C.

**Aaron Francis** [9:23]:

That is a great story. Yeah, I feel like back then, GitHub, you actually followed people and it was a social network, and then there were forums where people would actually hang out and talk, and you could really find some undiscovered talent in those places. And I don't know if that's still the case, but hey, it's worth looking into. So when you started as a Rails engineer, talk to me about some of the scale that you saw maybe in the second half of your career as you transitioned over to the infrastructure side or started getting really good at Postgres. What kind of scale growth did you see at Shopify while you were there, and what were some of those challenges?

**Simon Eskildsen** [10:03]:

Yeah, when I started working on infrastructure at Shopify, it was a couple hundred requests per second. And the last BFCM before I left was around a million requests per second. And on a Ruby on Rails cluster, this is quite sizable scale. And so I worked, when I worked on doing that, my time there was just making sure that everything in the core app and all the core database stuff just worked. And we were a team of comrades on the last resort pager. There were about six to eight of us, and we were just dealing with whatever came up. Some of us, I was often working on the more longer-term database projects, making sure we could run in multiple regions, make sure we could fail over a region with very little notice. All these kinds of things of just sharding everything as hard as we could on the Shopify ID. That's what I worked on for a very long time: everything caching, everything database, because that's the hardest part to scale when you're scaling something as quickly as we scaled Shopify. It was a journey of a lifetime. I also worked there. I worked on search. I worked on... me and Justine, my co-founder, we rewrote the entire Shopify storefront and then built a wonderful team to do that. And for performance and all the things that we learned about running Shopify, we got a rewrite to serve almost all traffic in 18 months for all of Shopify, even though the app is almost 20 years old. Just so many fantastic things we built. Nginx and Nginx Lua were just the secret weapons to run all of Shopify to shift traffic so that when Kylie Jenner was selling lipsticks, that, you know, Aaron, I don't know if you have a Shopify store, but if you have one, Kylie should not be able to take it down, right? And prioritizing traffic, multi-tenancy engineering. I would never have guessed how much this would translate into starting a database company, which I always felt incredibly insecure about. Well, because I haven't written a database. I've used databases for a very, very long time, but most database founders have built databases their whole career. But I've mostly just used them and bought them and looked at a lot of websites for them. I think our website is sort of the antithesis because a lot of the times when I went to a database website, when I was at Shopify, I felt like half the time I couldn't figure out if they were selling some new cool bespoke fashion product or if they're selling a database. And I just really wanted to know, like, what are the trade-offs you've made? What are the guarantees that you provide? What does it cost? How is this to run? What's the architecture like slotted into my mental model? And those are the questions that I hope that when people go to our website, they just feel that are immediately answered.

**Aaron Francis** [12:44]:

Yeah. Yeah. I want to get to turbopuffer for what it is. But just right before we do that, how fondly do you look back on your time at Shopify? Like when you look back on that, what are the feelings that you have?

**Simon Eskildsen** [12:58]:

I think the feelings are... There's a video of me that my dad has from when I was a kid where I just got off a roller coaster at Disneyland Paris and say, I say something along the lines of, "That was really fun. I never want to do that again." I think that's the feeling that you walk out of a hypergrowth generational company run if you have really just given it everything you had. That's how I felt. And when I stepped away from Shopify, there was some recovery to be done. And I don't think that I ever thought that I would step back into the ring and the fire again. But once you've experienced it once, you end up chasing it for the rest of your life, even if it's with some periods and different seasons of life in between.

**Aaron Francis** [13:56]:

Yeah, no, I totally resonate with that. I mean, that's how I felt after our first set of twins was born, and then we had a second set. So I get it. I get it. That was, yeah...

**Simon Eskildsen** [14:07]:

Wait—you have _two_ sets of twins. Four kids, and at one point you had two-and-a-half-year-olds _and_ two newborns at the same time. I don't know how you're standing here today. I don't know how you do anything. That is more remarkable than anything any guest has told you on this show. That is epic.

**Aaron Francis** [14:22]:

Yeah, never want to do it again—but here we are.

**Aaron Francis** [14:32]:

Yeah, needless to say, I resonate with that feeling deep in your soul of, man, that was awesome. Boy, am I exhausted. So I get that. So you come out of Shopify and you spend these three months at a time, you know, going around and helping friends, you know, dusting off some of your war stories and taking them to other startups. And then how long did that period last where you're doing this consulting? And then we'll get to turbopuffer.

**Simon Eskildsen** [15:00]:

Yeah, I think first, first I was just like, this is the summer where we just get the deadlift up as much as possible. And it was just like the summer of Simon. And I loved it. I was just writing napkin math posts and paddleboarding like a maniac and working out five times a week, and it was glorious. And then by the fall, I started getting bored with that. And that's when I started just sort of in three-month increments joining my friends' companies and vesting a bit of equity in return and helping them with infrastructure stuff. And the common thread was basically: at Shopify we used MySQL, so if you're using MySQL in the 2010s, you feel incredibly gaslit by the orange site that you made the wrong choice in 2005. It doesn't matter how good your business is; it could be better if you're using Postgres. And so I was very excited to finally get my hands on this thing. And it turns out that it's just a completely different set of problems. And one of the trade-offs is tuning the AutoVacuum. That's what I spent a lot of 2022 doing, was tuning AutoVacuum. And one of the companies I was tuning AutoVacuum for was this company called Readwise. And Readwise, what it is, is that it's a way to read articles and books and whatever later, highlight it, retain it, search in it. And they asked me if I could build a little recommendation system. And I'd heard of these things. I mean, I don't, you know, I'm like a database nerd. I don't really know anything about recommendation systems. So I just started researching, and I found this thing called vector embeddings, right? Where basically you feed an LLM a bunch of content, you chop the head off, and then out comes this like coordinate in a coordinate system. And magically, like coordinates that are adjacent in the coordinate system are also of content that is similar. And that was really cool to me. And so I read a bunch about that. How do you build these models? And so what occurred to me is like, well, these models are actually... these embedding models are trained on the articles because they're trained on the public web, so it must be really good. And so I built a very small recommendation engine over the course of a week that would sort of like take articles that you've read and then, you know, do vector embeddings, find other articles that were similar, and it kind of worked. Like, it was kind of interesting, and it was at least good enough that I got recommendations from one of the team members about articles that were around like pregnancy and so on. And I didn't know that his wife was pregnant, but it sort of leaked through the article recommendations because, you know, it was at least that good. But the problem was that, you know, sort of got something fine, wasn't excellent, but it was promising enough to go in this direction. And I ran the back of the envelope math on bringing this to production on one of the vector databases at the time, and it would have cost 30 to 40 grand a month. And this was a company that was paying three to four grand a month for their Postgres and maybe a couple other thousand per month for all their other infrastructure. So you're almost looking for them to spend an order of magnitude more to power the infrastructure for one feature. So we'd sort of entered this bucket of, "We'll do this later when the costs come down." And that problem haunted me because I looked at the pricing. Well, clearly, they're doing everything in DRAM and they're replicating it. It's like there has to be a better way. And so I maintained this repository. Have you seen the napkin math GitHub repository? Have you ever encountered this? It's essentially just a really shitty Rust script that I wrote that figures out memory bandwidth, disk bandwidth, like all these different sort of base numbers. And it's very useful because you can do things like, "Oh, this algorithm is taking four seconds, but actually we did, you know, it's traversing 20 gigabytes, and you know, you can do that in a second on a modern machine." So what's this 4x gap? And you figure it out. This, by the way, is way better method than profiling. Profiling just sort of like that is for iterative improvement. Napkin math is very good for finding order of magnitude improvements. And I just started doing the napkin math on building a search engine where you put everything on S3, everything, like nothing else, no Raft or Paxos or consensus bullshit, just everything on object storage. And it seemed like you could actually do it. The fundamental trade-off was that the write latency would be higher, and then once in a while when you queried something before you loaded it into cache, it would be a little bit slow, but you might be able to get it off. And so I just holed up in, you know, at my cabin in rural Quebec for the summer of 2023 and just forced through trying to find a way to make this architecture work because it felt very clear to me at the time that if I was not going to do it, then someone else was going to do it. And there's just these like S3 had only gotten the right primitives to be able to do this very recently. NVMe SSDs were recent-ish, right? And it felt like the perfect moment in the perfect workload to do this for. I couldn't articulate it very well at the time. It's like there's something here, and it was like trying to explain to my wife, and she's like...

**Justine Li** [20:21]:

I love you, Simon. I have no idea what you're talking about.

**Simon Eskildsen** [20:26]:

Yeah. And I released it in October, and it was like the fourth rewrite, and I was going manic, and there were so many things that didn't work, but it just needed to get out there. At that point, it was just like, "Fuck it, here it is." Can I swear? I feel like I've sworn a lot. Is that cool?

**Aaron Francis** [20:45]:

Go for it.

**Simon Eskildsen** [20:46]:

All right.

**Aaron Francis** [20:46]:

Everyone's an adult here. I think we only have 30 and older probably listen to this show, so it's fine.

**Simon Eskildsen** [20:54]:

Dropped it on X, and it got some good traction, and people were encouraging.

**Aaron Francis** [20:59]:

As an open source...

**Simon Eskildsen** [20:59]:

That's sort of... Sorry, no, it was not open source. I didn't have time to open source it. Yeah, it was a product. It was a SaaS product. Like, give me your vectors, I'll give you the results. It was just that. And it got really good traction. That encouraged me to keep going. And then a small company in San Francisco reached out, and they were having trouble with, "We're paying way too much per user to store all these vectors and code bases." And I'd never heard of this company before, but for whatever reason, something compelled me to go visit them. Probably it was because they were so busy that they kept not showing up to our scheduled calls, and I get it. And then so I went to San Francisco and had a great chat with them, and they decided to adopt this little database called turbopuffer, this little innocent thing. And of course, that company was Cursor. And yeah, just a very special relationship. And anytime I got the opportunity to, I would pass on as much knowledge as I had about tuning the Postgres AutoVacuum or anything that I'd learned from Shopify to them. They don't call me anymore because they're probably better at it than I ever was now. But it was a really fun relationship between two very early companies. Cursor was a lot less early than I was at the time, but they really took a chance on turbopuffer, and they really took a chance on me and Justine, my co-founder. And as all of this was transpiring, I was just thinking, who would be the best person that I know that I could do this with? And Justine just immediately came to mind. She was the one that I'd worked on so many incredible things with at Shopify, everything I'm proud of there, and even projects that neither of us are proud of, we've worked on together. And it was just she was just the first that came to mind, and I've never seen her do the things that she's now able to do. It's incredible.

**Aaron Francis** [22:51]:

And so this is an amazing story. And so you all, so you and Justine, is it the two of you? Y'all are the co-founders?

**Simon Eskildsen** [22:59]:

Yeah.

**Aaron Francis** [22:59]:

Okay, so you and Justine founded this thing, launched it a couple years ago, and somehow either the first or the first big or one of the first customers was Cursor, which is insane, by the way. Just insane that that was one of the first big ones. So backing up just a little bit, you're seeing... you've seen in your experience people struggling with search, recommendation, relevancy, that sort of stuff, and you're seeing these primitives come online with S3 and thinking these two things can meet, and it seems like there's something that should be done here. And you go off into the cabin and write it up, and it works. And so I want you to give me the elevator pitch for turbopuffer, and then I want to talk about what you discovered about S3 and kind of how you architected this whole thing such that you can make this work. So what is turbopuffer?

**Simon Eskildsen** [23:54]:

turbopuffer is an object storage... I mean, it depends a bit on who we're talking to in the elevator here, right? So we'll hit it from a few angles here. turbopuffer is an object storage-first database. It is a search engine that allows you to connect enormous amounts of data to AI. Examples are Cursor, right? And Notion, Linear, and many, many others. And the reason they choose turbopuffer is because we can index and connect more data to AI than they were able to do before. So Notion used to have a per-user AI cost, and when they moved to turbopuffer, they could take that away, and they could index more data than they had before. Cursor used to index in the thousands of files before turbopuffer, and now they index as many as they can find. And the list goes on, and we help our customers realize the most ambitious versions of themselves, their products.

**Aaron Francis** [24:51]:

Okay, I love it. So a couple of things stood out to me in that succinct pitch. One is object storage, obviously. That is unique. And the other is you said both database and search engine, which don't always go together, right? And so talk to me about, like, fundamentally when you were starting this, what was the thing that allowed object storage to work here where maybe three, four, or five years before, object storage was like, "No, that's never going to work." What was it that you saw about object storage that changed your mind?

**Simon Eskildsen** [25:25]:

I'll explain this in a way that I like to think about it now in that if you want to create a big database company, you need two things. The first thing that you need is that you need a new workload. If you want to create a really big database business, you need more or less every single company in the world to have a use case for your new database or at least consume one or more products that have a strong use case for your database. That's ingredient number one if you want to build a big database company. The second thing you need to build a big database company is that you need a fundamentally new storage architecture. Because I can promise you that if you have a new workload, all the other database companies are going to look at that new workload and say, "I want that." And there's no good reason why they shouldn't get it and fragment the market unless you have a fundamentally new way to store the data that they can't rewrite everything to do because it would screw up their guarantees and trade-offs. The new storage architecture is to store everything on object storage and then puff it up into NVMe SSDs and DRAM as you query the data, almost like a JIT compiler. That's the new storage architecture. It is only possible now because of three things that have changed. The first thing that's changed is that NVMe SSDs are available in the clouds as of around 2017 is when they launched in AWS clouds, and even then there were very few of them and very few SKUs. NVMe SSDs are about 100 times cheaper than DRAM, but only around five times slower if you use them correctly. Using them correctly means that you have to have a lot of outstanding requests for every round trip, right? So you can't just use them like you use DRAM, like, "I want this, and I want this, and I want this in random order." You have to say, "I want this one-terabyte chunk, and then I want this other one-terabyte chunk," and you have to do all of that in parallel requests, and the storage engines have not been built for that. The second thing that changed with storage architecture is that S3 only became consistent in 2020 at AWS re:Invent. So that would have been December. This is completely overlooked because everyone just assumes this, but that is not that long ago, especially not in database land where it takes a long time to mature. The third thing that happened that's very important is that S3 gained compare-and-swap (conditional writes) for object metadata remarkably recently. And what compare-and-swap allows you to do is to not have a separate consensus layer on top of your database to ensure metadata has not been changed in the interim. That means you don't need another metadata layer. That means that you can build a database where you just run everything on stateless nodes and the only state is in object storage, which means it is more cloud-native than any other database architecture that we can think of. And it's also fundamentally the cheapest way to run a database in the cloud. Those are the three things that changed that allowed the new storage architecture. That's what makes the economics and the underlying architecture is fundamentally different than MySQL, than Snowflake, than Databricks, than MongoDB, than all the existing databases. Those are the two things you need.

**Aaron Francis** [28:50]:

That is very helpful. Describe to me this. You said you puff it up. Describe to me that part there. So where is it long term? And then what is this process by which maybe you can do request lifecycle, you can do search query, whatever example you want to use, but describe this process that the data goes through to fulfill the user's needs.

**Simon Eskildsen** [29:13]:

Yeah. So what happens? When you do a read, you will basically go to a load balancer, and the load balancer will say, "Well, what table is this for?" We call this a namespace. The namespace is logically just a directory on S3, right? So it could be one could be called AaronDB, and one could be called SimonDB. It could also be, you know, Notion customer N, or it could be Cursor code base Y, right? These are the prefixes. We call these namespaces. It's a new database construct. So if I am querying AaronDB, then I am going to the load balancer. The load balancer will hash AaronDB and then send me to node two out of 128 or whatever. It will look at the DRAM cache and see if it's there. And if not, it will go to the NVMe SSD cache. If it's not there, then it will go to object storage and get the blocks directly. turbopuffer's database engine is completely optimized for object storage to the point where when we get data from the database, we do a range request directly into the S3 file of exactly the data that we need because we know where it is in the file. A lot of databases that have tiering will basically download an entire file from S3, hydrate it into cache, and so on. But we can operate directly against S3. When you have a cold query that misses DRAM and misses the SSD and goes directly to object storage, the latency is around a second, 500 milliseconds to a second, depending on how hot the S3 prefix is. If it's on disk, it's often less than 100 milliseconds. And if it's in memory, it could be around 10 milliseconds. So it's sort of like these are the order of magnitudes of where the caches are, right? 10 milliseconds, 100 milliseconds, and about a second. In practice, it can often be faster, but that's a useful way to think about it.

**Aaron Francis** [31:03]:

How do you know when you're going to... let's say you fall all the way through and you got to go to S3. How do you know which range of that file to get? Like, who told you that? And second question, what is in those files? What is the data that you're storing in those files, and how are you storing it? Like, are you breaking it up? Is it like compressed? Like, what does that actually look like in there?

**Simon Eskildsen** [31:31]:

It might be useful first to talk about turbopuffer V1, like that thing that I hacked up at the cabin in Quebec, and then, you know, brought poor Justine in to help optimize it before we could get to V2. V1 was... You take all the vectors and you cluster them. So should we do a little detour here into what a vector index is?

**Aaron Francis** [32:00]:

Is now a good time for that detour?

**Simon Eskildsen** [32:02]:

Yes—please. The simplest way to do vector search is that you have a... well, first maybe I'll explain even what a vector is. So we got a little bit into this, but I was feeling funny before in the way that I explained it, so we'll do it properly now. The way that I usually explain it, and you should ask follow-ups here because we're backing into a long-winded answer to your original question. A vector is a point in a coordinate system that represents a piece of data, and the point is adjacent to other things that are similar, right? I'm standing in front of a table. So the table would be in the coordinate system here, and right next to the table would probably be a chair, but closer to the table might be a dining table, right? So you imagine that you train a model that is very good at taking content that is similar and putting it in the coordinate system.

**Aaron Francis** [32:56]:

One clarification before you go on. I'm picturing XY—table here, chair there—because I'm human and two dimensions are intuitive. But in reality it's not 2D or 3D; it's hundreds or thousands of dimensions—768, whatever. Is that right?

**Simon Eskildsen** [33:31]:

That's correct. I used two dimensions because I can't visualize 768. I'd need your twin superpowers for that.

**Aaron Francis** [33:45]:

Hard pass.

**Simon Eskildsen** [33:46]:

Fair. Want me to explain it in one dimension instead?

**Aaron Francis** [33:51]:

Yeah—that would help.

**Simon Eskildsen** [33:54]:

Exactly. And dear listener: when we say 2D or 3D here, read that as "a huge number of dimensions."

**Aaron Francis** [34:01]:

Okay—keep going.

**Simon Eskildsen** [34:03]:

Let's just like, just to really hammer it home. Imagine Spotify. They have all of their songs in a big coordinate system, and you can imagine a rock cluster and a pop cluster. You zoom in and you find little small clusters in there. That's how a simple, the simplest form of a vector index works. You basically cluster it so you find patterns in the coordinates. And then you say, when I'm searching for a song, I'm just looking at the clusters that are similar. You basically take an average of everything that's inside of a cluster, so the average of all pop songs and the average of all rock songs, and you search for your query vector and say, "Well, it's closer to the rock, so I'm only going to look at the rock songs." And suddenly you're searching 50% less data.

**Aaron Francis** [34:42]:

So your universe got a whole lot smaller.

**Simon Eskildsen** [34:45]:

And that's how you build a vector index. There's a lot of challenges with that; we could get into that, but that's the simplest way. So what turbopuffer V1 did was that it basically took all the data and then it built a bunch of clusters: rock, pop, hip hop, whatever, and then it created something like `centroids.bin` on S3 for a namespace—think `AaronDB`.

**Aaron Francis** [35:10]:

So who... where was that? Like, let's say that I am... I'm giving you all of my music. Am I locally creating vectors and then I'm throwing vectors over the wall in V1? Am I throwing them over the wall to you and then you cluster them and centroid them? Or like, who's doing what where?

**Simon Eskildsen** [35:27]:

That's right. You are just like... this is like Stripe for vectors. You're just sending a vector, and then you can search the vectors, and that's really all you can do. There's nothing else really to do. So you send all the vectors, and then turbopuffer takes all the vectors and it clusters them, and then it creates... then it takes the average of every cluster and uses that centroid and puts it in centroid.json. We'll just call it that. It wasn't actually JSON, but just like... so it's simple. So centroid.json is basically just an array of arrays, right? Where every array is sort of like rock and then a centroid, pop centroid, whatever. And then we have another file that's called cluster1.json, cluster2.json, cluster3.json, and the centroids map back to that. Now, the way that you serve the query is that you go to S3, you get AaronDB slash centroids.json, you look for the closest centroids to your query vector. And then you know, okay, well, you know, this is sort of in between like pop and country, whatever, there's probably some artists there, I'm sure plenty. And you look up, that was Cluster 79 and Cluster 108, and you download those two files, you search through those, and then you return the results. Two round trips for S3, P90 to S3 is maybe around 200, 250 milliseconds, query's done in 500 milliseconds.

**Aaron Francis** [36:50]:

Okay, so I think I see where this is going, but I'm not going to jump ahead. So let me say it back to you to make sure that I have it right. So I, as the user of turbopuffer, create vectors however I want, and then I send them over the wall to you, and I say my namespace, or maybe you define the namespace, doesn't matter, but you put them in a namespace of Aaron's database, and you create from all of my vectors, you kind of create like an overview, like a world map of the clusters, and maybe there's a hundred of them. And in centroids.json, you have a listing of all hundred of those centroids. And then I send a query over that's like, "Hey, I want to find more artists like Taylor Swift." And you go to centroids with Taylor Swift and say, "That's pretty much pop most of the time." And then you go grab the pop music JSON file, pull it back, and then search through the pop music to look for other artists that are like Taylor Swift and then send that home to me. Good?

**Simon Eskildsen** [37:50]:

That's right—that's right.

**Aaron Francis** [37:52]:

Perfect.

**Simon Eskildsen** [37:54]:

So you weren't deep in the weeds on `centroids.json` and `cluster_1.json` through `cluster_128.json`? That was more or less V1—and honestly, "it worked" is the right summary: Cursor and Notion ran on it through the end of 2024. There was a lot less "good" in that implementation than we'd want in hindsight, but it worked. It worked, and it was, you know, Justine did all this crazy stuff because then, you know, I got ripped into B2B sales, you know, everyone's dream, and Justine was deep in the code mines just like making all the shit code that I'd written zero copy and all this craziness to just make it perform. Then we hired some people who actually knew how to build databases. And, I mean, Justine just by her remarkable mind knew how to build databases a lot faster than I did. But we started working on V2 in the spring of 2024, so about six months after launch. And this is where the sort of search engine versus database comes in. V1 is very much a search engine because it can only search; it can't do anything else. V2 was a database that is excellent at being a search engine. A search engine to me is an attribute of a database, but a search engine is not necessarily a database. To me, a database is something that in the limit could do any SQL query in a great way, right? It has a query plan for that. So V2 is like a proper LSM. There's a key space. We implement all of the vector indexing and this clustering is on top of the key space, but underneath turbopuffer is just a KV store, and then we build all this stuff on top. The LSM engine is optimized for object storage and all the trade-offs of that, which are different than doing it in traditional architecture. And that's how we... that's what we would call the database. Now, of course, you can't implement every SQL query in the span of six months on a new KV store, so the focus of turbopuffer is very much to be excellent at search. But over time, turbopuffer is supporting more and more queries, right? We have aggregation, faceting, richer attribute filters—all the database-ish affordances people want on top of search—but our core remains to be an excellent search engine at indexing enormous amounts of unstructured data. So now we can do, of course, full-text search and all these other things.

**Aaron Francis** [40:22]:

Can I ask why? Full stack search makes sense to me; that's search realm. Can I ask why you did all of those other things? Because that sounds like a lot of work, first of all, and it sounds like search was the original thesis and very, very hard. So why add on this other thing that is also very hard? Who was asking?

**Simon Eskildsen** [40:44]:

So what you asked for when you have a search engine, basically when you have your data in a database, you start to want to just do anything with that data that's in the database. So, I mean, we get asked for all kinds of questions and very complicated, you know, probably someone's going to ask about CTEs and recursive CTEs, that God forbid at some point, right? So people will ask all those, but of course you have to roadmap it. And the reason that you want aggregations is because you want to do things like, "Well, I kind of need to know how many things are in the database," right?

**Aaron Francis** [41:17]:

Okay—that's the easy buy-in.

**Simon Eskildsen** [41:20]:

It's like, okay, that's useful. And then someone is... someone says, "Well, you know, it's really nice on an e-commerce site when you see... when you can toggle the sizes and you can show these, the facets in the sidebar." Well, that's a group by count. And then, you know, you're like, "Well, I kind of want to know how many matches are in each document because I'm searching for these chunks in the document or pages, and I need to... these are aggregations." So it's not like people right now are saying, "Hey, I want to do my revenue reporting on turbopuffer." It's that you have this data in turbopuffer, and you want to do very reasonable things that are still in the area of search. And so those are the kinds of queries that we are prioritizing. But over time, there's going to be lots of them.

**Aaron Francis** [42:09]:

So is it fair to say that you implemented a lot of this standard database stuff to solve the business search need, even though it is not the technical search need? Does that distinction make sense? Like, as people are doing the business side of search, faceting is a great one. And faceting, for the listeners, when you're like, "I want red shoes," and then like Nike drops down to 50 pairs and Adidas drops down to 25, and you're like, "Red and size 12," and then it's like Nike has two pairs, and you're kind of recalculating all this stuff on the fly. That feels like the business domain of search, but under the hood, that's the technical domain of like, "Well, that's just kind of more traditional databases." Is that fair to say that distinction there?

**Simon Eskildsen** [42:52]:

Um, I think so. I think it's just that people just need certain... you know, I think of all queries as SQL queries, and there's just a bunch of SQL queries that you expect out of a search engine. It may not look like SQL because if I shipped SQL to turbopuffer right now, if you, Aaron, were using it, you would just get really annoyed at all the things that we don't support. But all...

**Aaron Francis** [43:15]:

Right—the `WITH` clauses, CTEs, recursive CTEs, all of it.

**Simon Eskildsen** [43:17]:

Recursive CTEs are a godsend—you can do wild things with them. I've basically implemented spreadsheet logic with recursive CTEs—while loops in SQL. DuckDB is very good at them.

**Aaron Francis** [43:27]:

Someone over there really nerded out. I don't think MySQL even had them for the longest time.

**Simon Eskildsen** [43:34]:

MySQL 8 does, at least—I don't know whether the test suite covers every edge case—but the broader point stands.

**Aaron Francis** [43:42]:

Yeah.

**Simon Eskildsen** [43:44]:

There are a lot of things people expect from a search product that end up looking like traditional database queries, even when the API isn't SQL.

**Aaron Francis** [43:53]:

So that explains to me a little bit of like the difference between V1 and V2 of like scope and kind of like, I guess, thesis or mindset. But can we go back to the naive S3 implementation on the first one where you just had to do two round trips every time? And can we update that knowledge for V2 on...

**Simon Eskildsen** [44:16]:

Yes.

**Aaron Francis** [44:16]:

How does that look in—what version are we on? Still V2? V3?

**Simon Eskildsen** [44:20]:

We're kind of on V3 in some ways, but I'd mostly talk about V1 and V2. We started calling it V3 when a customer needed to search on the order of a hundred billion vectors—

**Aaron Francis** [44:41]:

—which is hilarious.

**Simon Eskildsen** [44:41]:

—and pretty hard. So we need adjustments to fit that scale. Anyway, V2 is the more general design—there you're a little bit more civilized about these things, and it's not so tailored to one use case. And so in V2, what you do is that you have a key space, right? It's a... and so you might have as key, you know, cluster... instead of cluster 128 being a JSON file, cluster 128 is a key. And so you have to figure out where is that key, right? And so you download some metadata file, and that metadata file has an idea of what files contain what ranges. I'm really trying to simplify here, right? But you maybe download LSM.json. This is not how it actually works, but it's like pretty close. LSM.json, you get that, and then it says, "Well, in these files, these are the files," and maybe some metadata around what key ranges are in what files. And then you go to file, you know, the file is just like uuid.json. It's like some big file somewhere, but it's not a JSON file anymore. And you do a round trip to that file where you get the index block, and the index block is the last, say, 32 kilobytes of that file. You download that in another round trip, so you're at two round trips now.

**Aaron Francis** [46:06]:

And why that decision? Are those files so big that it makes more sense to just get the index, calculate what other range you need to grab from that, and go back and grab that range right back?

**Simon Eskildsen** [46:56]:

That's... those files can be gigabytes large.

**Aaron Francis** [47:00]:

Gotcha. Okay, that makes sense. And then, so as you've described it, we're still just operating with, you know, turbopuffer is going straight to S3. And so then do we put those other stops in the middle, and you just have intelligent caching along the way so that you're not grabbing whatever was formerly centroids.json off of S3? You're not grabbing that off of S3 anymore. That one's probably hot all the time. The metadata is probably there all the time. And so you can just get that directly super fast, and then the caching layers come into play. Is that correct?

**Simon Eskildsen** [47:29]:

In the simplest form, I mean, the caching is ever-evolving, and the smarter the caching we make it, the better performance our customers get, right? You can imagine that caching that small LSM.json is really useful to have in cache almost indefinitely. That saves you that first round trip for a very small file. So we could prioritize things like that in the cache, but in the simplest implementation of the cache, you can essentially just imagine that the NVMe SSD, we create one file that's the size of the disk. You can imagine just as cache, that's just the name of this file, and you just put the keys in, and you just load it in as a ring buffer. So it's like, you know, you get one gigabyte, you put it at the beginning of the file, another gigabyte, put it at the beginning of the file, and you just do that. And then at some point, it wraps around, right? You just start overriding from the beginning. This is a very great way to use a modern disk because you're just writing as fast as possible. You could do lots of heuristics where, like, when you wrap around, right, and sort of the... it starts eating itself, you could start to say, "Well, actually, this is accessed a lot, so let's not override that." You could do all these kinds of things, right? And make it arbitrarily complicated. But turbopuffer, like, turbopuffer just shipped in the beginning with just the ring buffer. It didn't do anything smart, and I think Justine had just had a 200-line implementation of this, and it was rock solid.

**Aaron Francis** [48:53]:

That's awesome. That is so cool. I love a simple solution that just works all the time. So what are the... you talked early on about the turbopuffer website, or rather you talked about the fashion websites of databases in the tens, right? And how they just like don't really tell you what's going on, like you just look sexy. So what are the trade-offs of turbopuffer? What are the decisions y'all made that you want to like illuminate? Because obviously it's a very different mental paradigm than something like a MySQL, Postgres, SQLite, anything like that. So talk to me about those trade-offs and what decisions you made to make this search case actually work.

**Simon Eskildsen** [49:40]:

The biggest thing is that turbopuffer has very high write latency. If you write to a MySQL... your write latency is essentially whatever an fsync is. On a modern flash drive, you can do an fsync in a couple hundred microseconds, maybe a millisecond if it's a slow disk or network disk. That's pretty fast. And MySQL and Postgres and so on, you can even get it faster than that because of a variety of things that they do. But generally, it's sort of in the hundreds of microseconds ballpark. turbopuffer is two orders of magnitude slower than that. Our writes take in the low hundreds of milliseconds because that's the latency you get to S3. If you were building the Shopify checkout with hundreds of milliseconds of latency on every write in the checkout path, it's just not going to work. Like, you're just not going to buy anything. And that's the fundamentally biggest trade-off that we make. The other trade-off, which is more of a trade-off with the current implementation of turbopuffer than a fundamental trade-off, is that occasionally you will get a cold query. Occasionally, you will do a query, and it will go directly to object storage and then puff into cache. That doesn't have to happen, right? We can guarantee that there's just always hot on at least one or two nodes, and you could do replicas and things like that, and we want to allow users to control that type of thing down the line. But it is a fundamental trade-off that you make in turbopuffer right now, which means that your tail latency could be close to a second. There's lots of things you can do when you open a Q&A pane in Notion; they send the request to turbopuffer to start warming the cache before the user has started typing in again to get around these kinds of things. And we try to always improve the intelligence of the cache, right, to make sure that it's evicting and keeping things in cache that are used. Those are the two fundamental trade-offs: high write latency means that very complicated transactions and things like that, it's just... you're just not going to do that. But that's the fundamental trade-off.

**Aaron Francis** [51:43]:

I buy that. So is cache warming a primitive you expose? So like if I have... we'll go back to music and say it's like, you know, post-rock shoegaze or something that doesn't get pulled into the cache very often. Does Notion, for example, just send off a query for post-rock shoegaze, or is that a primitive you expose? It's like, "Hey, warm up the cache somehow, some way."

**Simon Eskildsen** [52:05]:

Yeah, we expose `hint_cache_warm`—basically a hint to prefetch hot keys, like `madvise` for our cache. If it's already in cache it's free; if not, we charge you for about one query.

**Aaron Francis** [52:12]:

Love it—that's a real primitive. So S3 has dozens of competitors, some very popular, some that are just upstarts, but everybody claims S3 API compatibility. That doesn't super matter for y'all because it sounds like the underlying part is what super matters for y'all. Are there any other object stores that offer the same kind of performance that you've looked at that you're like, "Hey, maybe this is an option that maybe has better performance or different performance"?

**Simon Eskildsen** [52:55]:

turbopuffer started on Google Cloud Storage because Google Cloud Storage had compare-and-swap before S3 did. I don't know when they got it, but they had it in 2023, and S3 didn't. So we started on Google Cloud Storage, and we were so adamant of not having a metadata layer, so adamant that in the beginning when we sold, Notion is in AWS, and we were in GCP, and we bought like dark fiber interconnects with GCP because we needed this compare-and-swap primitive so badly, and Notion was like, they understood. So, and unfortunately in Oregon and US West 2 or 1—whatever, doesn't matter. Those two data centers in Oregon are physically around five milliseconds apart. Like if you'd like have dark fiber, like in reality, it should be less than that, but let's say five milliseconds. But they were showing up as 20 milliseconds going through Seattle. And so they're both like one, I think AWS is like in some suburb of Portland, and the other one is like, like the GCP one is like out in the boonies, and they were going through an exchange in Seattle, and it was like 20 milliseconds. So we bought like... we bought through an exchange in Portland, setting all this up. Like these were the lengths, and we were tuning. We were tuning so much TCP because TCP by default, when you request data, you only get about 15 kilobytes of data, and then you have to wait for a round trip, and then another round trip, and then you get double, like this TCP slow start. And so we were tuning that like on both ends. We were setting a... like we did all this crazy stuff because Justine and I have been on call for systems where downtime costs customers millions a minute—and we were not going to hand-wave tail latency on top of object storage. So we bought dark fiber, tuned TCP, did whatever it took to make the path fast enough. Now, of course, we don't have to do this as much because we also run on AWS on S3. But to go back to your question, there's Google Cloud Storage, there's S3, right? There's Azure Blob Storage. We're starting to work with customers in Azure. Sure. I don't have too much to say about Azure Blob Storage yet, but between GCS and S3, my main observations are that the tail latency on S3 is higher, but the small object latency on S3 is lower. It's about eight milliseconds. If you're getting a very small object from S3, it's faster than GCS. GCS for small objects is around 15 to 20 milliseconds, but the tail latency is better on GCS. Other than that, they're very comparable systems in our eyes.

**Aaron Francis** [55:25]:

So you've already gotten to the other big clouds, so that would leave, I guess that leaves many. I know DigitalOcean has a compatible one, Cloudflare has a compatible one. I just talked to Tiger Data, and that'll be coming out, at this point that y'all are listening to this, that'll have been out for a week or so. So that's another one. But it sounds like you've got some of the big ones that your customers are looking for right now. How hard is it to maintain? How different are those primitives? And do you just have like, you know, adapters where in somewhere in the code, it's like, "Hey, I'm running on..." and on GCP, do this one thing slightly differently.

**Simon Eskildsen** [56:01]:

Yeah, we have our own client that we've written that works with all of our layers, and it's... I think you want to own your own client when you're doing this kind of stuff. There's a lot of things that you have to do, like signing the request. That was two days of my life that I never get back. And there's a lot of small minor differences, but there's just if and else statements. But Azure Blob Storage is particularly annoying because they do not implement the S3 XML spec, so you have to do something completely different. And in terms of others, it will come from customer demand, right? If customers are asking for them, we will. Originally, turbopuffer only worked on top of Cloudflare R2 and Cloudflare Workers—believe it or not, that was the absolute first version. There's probably still some WASM-era code sprinkled around the codebase. That's actually why I had to write my own S3 client: the existing ones didn't work well in WASM. But I couldn't get it fast enough there, so I moved on to this architecture.

**Aaron Francis** [57:06]:

So that's up to four major clouds that you have at least at one point written it for. That's kind of wild. So where do we stand right now? So turbopuffer, who do you serve? Like what workload, use case, business type do you serve the most? And what do you see in the next six months, a year for turbopuffer? Where do you want to go?

**Simon Eskildsen** [57:30]:

We just want to get the whole world puffing—I should be hitting a vape on this call for full effect. Anyway—look, you can compile all of the world's knowledge into a couple terabytes of weights, and that model is going to have a very good idea about how to reason with the world. But in order for a model to reason with the outside world or the inside world of a corporation, it needs to search. That is the most important tool that it has in its arsenal. And turbopuffer is going to index all of the private data in the world that wants to be connected to AI. So when we say that people want to connect data to AI, that's what we do. We also connect humans to data at scales that were otherwise before very, very difficult. So we see that in like so many different types of businesses, right? There's code, there's legal, there's hedge funds that use us, there's all kinds of businesses that have massive amounts of unstructured data. And that unstructured data, everyone is trying to get value out of, and that's what AI is so good at. And it's also what search is really, really good at sifting through. And so the way that I think about it is turbopuffer is you give turbopuffer the haystack, and we will give you the chunk of hay that the needle is in. But the LLM is very good at then using the needle and using that chunk of hay and getting value out of it.

**Aaron Francis** [59:00]:

That's a fantastic analogy, by the way. That's great. Okay, that makes sense to me. All right, so what do you see like product-wise? Give me a sense of where turbopuffer is at and any sense of scale you want. Employees, anything public you want to share, like give...

**Simon Eskildsen** [59:16]:

Yeah, we're just shy of 20 people. We are a very focused team of people who just love to build databases and people who want to love helping people build amazing things with those databases. And those are the kinds of people that I think would love working here and are working here now. We power more than a trillion vectors. I haven't heard of anything else than that. I'm sure the hyperscale is about that scale, but to give you a sense of scale, the entire public internet is in the hundreds of billions, depending on how you do it. So this is extremely sizable scale. At peaks, we do more than 10 million vector writes per second, and we do tens of thousands of queries per second, which to me is not as impressive. Like, you know, having worked on large MySQL clusters that do millions per second. So these are the kinds of numbers that we operate at. So this is real scale, like it's like on some attributes larger than the scale that I was at that I've worked with Shopify. And yeah, we have many, many, many customers that work with us and trust us.

**Aaron Francis** [1:00:28]:

Okay, so you're two-ish years old, 20 people doing huge scale. I want to talk about where you're going, but just on the employee thing, there are a lot of nerds listening to this. Are you actively looking for people? And if so, what types?

**Simon Eskildsen** [1:00:43]:

We are always looking for people to join the team. We're looking for customer engineers. We're looking for sales. We're looking for database engineers. We're looking for people to work on the dashboard—more or less every role for someone who wants to help build a pretty database.

**Aaron Francis** [1:00:58]:

Love it. People can track you down if they're clever—but they should really go through **jobs** on the site, right?

**Simon Eskildsen** [1:01:10]:

You should be able to find me, and I think you will have a higher probability of getting a great answer if you go to jobs on the website because then it doesn't land in my inbox, which is getting a little bit hard to stay on top of.

**Aaron Francis** [1:01:23]:

Fair enough—you all heard that; hit the jobs page if you're interested. What's the roadmap for the next six months or year? You want to get the whole world puffing—are you mostly refining and hardening the core, or is there more search-adjacent surface area you plan to pull in?

**Simon Eskildsen** [1:02:03]:

Look, we want to... right now we are focused on building the best search engine and making that scale. So we work a lot on performance. We look closely at our customers' query plans and expand them. We are very focused on more full-text search features. One of the people we just hired has been committing to Lucene for more than 10 years, and we're working on just adding more and more text features to the product. We are working on puffing up the dashboard, so all the UX around using the product right now, if you log into the turbopuffer dashboard, you will feel like this was vibe coded by a 14-year-old. And that's right, that's because this was sort of written the initial version by me over a few months when I was early on at turbopuffer, and then an amazing support engineer has worked on it since in between answering customers. But it's starting to get some love, and I think people should get really excited about that. But it's really just we want to build a really good search engine, and of course we want to expand into helping with many of the workflows around search, but we really are focused on creating an incredible product at the core before expanding with more auxiliary offerings around it.

**Aaron Francis** [1:03:24]:

Okay. Yep, that makes a lot of sense to me. This has been great. You're really good at explaining all of this highly technical stuff. So well done you. As we wrap here, tell the people why they should consider turbopuffer and when they should consider turbopuffer. This is your like no holds barred, give us the pitch.

**Simon Eskildsen** [1:03:50]:

If you are searching data and you don't have that much data, then you should not be using turbopuffer. You should just do whatever you have right there, put it in pgvector or whatever you're already running—until scale and economics push you toward a specialized search layer.
