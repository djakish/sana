# Memory, evals, and efficient storage in AI systems with turbopuffer and Braintrust

September 11, 2025•Bessemer Podcast

[Video 3](https://www.youtube.com/watch?v=sYsvW0jBYm0)

## Transcript

**Talia Goldberg** [0:00]:

All right, team, let's get started. Welcome to Research to Runtime. This is a session on building AI systems and agents with our esteemed guests, Ankur, the founder and CEO of Braintrust, and Simon, the founder and CEO of turbopuffer. For those that I haven't met, I'm Talia Goldberg. I'm a partner at Bessemer in our San Francisco office where I lead many of our AI investments across stages. Bessemer is a global venture firm. We partner with entrepreneurs from the very earliest days through every stage of growth and are very proud to work with companies like Perplexity, Anthropic, Fall, Abridge, Canva, Shopify, ClickHouse, and many others, including those that are customers of Braintrust and turbopuffer. We've seen the power of these products and what they're enabling firsthand, so we're super excited to share that with the broader group. With me, I have my colleague Bavik, who's an ML engineer by training and also works with me to lead our AI investing. We really started this series because the landscape is a roller coaster—there's a roller coaster of progress, innovation, best practices, and everyone's trying to figure out what to do and how to build and best practices and tactics. It has been awesome to get this community together to share that and to learn from some of the best. With no further ado, let me let our guests introduce themselves and maybe give a little background on yourself and your company. Ankur, why don't we start with you?

**Ankur Goyal** [1:29]:

Yeah, sounds great. Very excited to be here and chat with you all. I'm Ankur. Prior to Braintrust, I used to lead the AI team at Figma. Before that, I started a company called Impura where we did AI document extraction in the stone ages, pre-ChatGPT, when it was quite hard. At both companies, every time we changed something—like updated our models, changed our prompts, or changed the underlying architecture that we used—we would break stuff for customers. We had to get really good at avoiding that. To do that, we built tools to help us do evals well. It was really hard to get data to do those evals, so we built observability tools to help us actually collect data in a way that was useful for evals. The third time around, that turned into Braintrust. I was reflecting this morning, and I actually only know two Simons, and I really like both of them. The other Simon is at Notion, and that Simon was one of the first people that we talked to. He shared how they did evals at Notion, and we started working with them—Zapier, Scribe, Instacart, Airtable, and a bunch of other really great companies that are building AI products. We were doing it really early, and by working with them, we collaborated and established some really good workflows around evals and observability that are now the Braintrust product.

**Talia Goldberg** [2:47]:

Awesome. And Simon?

**Simon Eskildsen** [2:50]:

Yeah, I'm Simon. I spent almost 10 years building infrastructure at Shopify. I'm up here in Canada, and when I joined Shopify, I was doing a couple hundred requests per second, and by the time I left, we had seen peaks of around a million. I worked on mainly the things that did not scale, playing whack-a-mole on all the bottlenecks when the Kardashians rolled through and did some of the largest flash sales in the world. The fundamental bottleneck for most major SaaS platforms is the database layer, so I more or less worked on every single aspect of the database and scaling the compute layer for almost 10 years at Shopify. When I left, I was bopping around at some of my friends' companies, helping them with little infrastructure things. At one of them, I discovered the massive cost of embedding-based search. With this one company, a company called Readwise, we just did some very simple article recommendations over the course of a month. It worked pretty well, but this was a bootstrap company that was spending $3,000 a month on their Postgres, and putting all of this into actually operationalizing all these vectors would have cost them $30,000 a month, and they actively did not do it because it was too expensive. That's what we set out to do with turbopuffer: to do an order of magnitude cost reduction to unlock a lot of this product that people wanted to ship. That's what I work on today.

**Talia Goldberg** [4:09]:

Amazing. Thanks for sharing that and giving a little bit of the background. Building off of what you just said, Simon, and just as a little bit of context for folks, what are the specific decisions in traditional vector databases that create these cost explosions, and what were the things that you guys did to really address that? I think it's known or publicly reported that folks like Cursor have 20x cost reductions by switching to your architecture, and it's not just cost; there's also latency and speed. What did you do, and what were the challenges?

**Simon Eskildsen** [4:44]:

Yeah, I think it became clear when you start a company, you generally have some insight or something that you think could be done differently. I think the insight that led to turbopuffer was that there was a new storage architecture in the air where we could use S3 or GCS or Azure Blob Storage as the source of truth. That only really became possible in the past few years. The new storage architecture that was in the air was one where you use NVMe SSDs. NVMe SSDs are about 100 times cheaper than using memory, but the throughput that you can drive through them is only about five times less. So if you can build a database that takes good advantage of it, you can run into some real economic advantage. The second thing that happened was that S3 became consistent at the end of 2020, and S3 got compare and swap at the end of 2024. This has allowed us, with these three new principles, to build databases that can have a completely different storage architecture than those that came before—one where S3 or Google Cloud Storage are the only source of truth. You don't even need a metadata layer. I think Ankur and I can both go into a lot of depth on how to use the metadata layer, and I think we have some different thoughts on how to do it, but this is a new storage architecture. I think fundamentally, if you want to build a generational database company, you need two ingredients. The first one is the new storage architecture because if not, then all the incumbent databases are just going to add on and eat you alive. But if you have a new storage architecture, it has fundamentally new economics or new performance characteristics, and you have a new workload that means that people are out shopping for a new database—in this case, connecting enormous amounts of data to LLMs is the general new workload. If you have both of those ingredients and you have good execution behind it, you have the potential to create a generational database company. I think that we saw those two things in the air. We talked about Cursor, Notion, and Linear—some of our customers—and for a customer like Cursor, it matters a lot what it costs, not because Cursor is cheap, but because Cursor has to earn a return on all of the data that they have to search over. That's fundamentally what we're all doing, right? We're earning a return on whatever is underneath us. If we can change the economics in a way where the products that people can ship on top of search are fundamentally different, then our customers can ship more ambitious versions of their product. With Cursor, we reduced their cost by 95%. That doesn't mean that we're the H&M of search; it means that suddenly they could index much larger code bases for way more customers on economics that they could earn a return on. Same with Notion—they used to have a per-user AI cost, and part of them removing that was switching to turbopuffer to go from a more traditional storage architecture into one where we could reduce their spend by millions of dollars a year on this new storage architecture. It made a lot of sense for them. I think I'm more in the business of making sure that people can earn a return on top of us with the product that they build than anything else.

**Talia Goldberg** [7:46]:

Yeah, I love how you talk about this very clear moment of "why now," because it's really hard to build a database company, as you said, and it's really hard to have these switching costs, and folks get very built in. What do you see for companies like Notion or others, or even a company that has an existing setup? How do they switch over, and what is that process like to get going on turbopuffer?

**Simon Eskildsen** [8:08]:

There's a couple of profiles, but I think there are two major categories. The first ones are more net new workloads—these are either newer companies or ones where they only have very simple search, and they can wholesale replace it with a simple hybrid search inside of their app, using normal lexical search on top of and then vector embeddings as well and fusing it together. These are more net new, where it's very simple for them to do it. They generally are newer adopters and then start on turbopuffer and then go from there. The other type of customer that we see are ones where they have existing lexical search systems, typically Lucene-based search systems that are tuned for lexical. They see that they can boost their search recall and precision by 10-20% by incorporating embeddings, which is huge, right? Some of these companies will have people that spend an entire year improving search results by one or two percent because that really ends up mattering on average to their customers. So 10-20% that we've heard from some of our customers is enormous, especially when you also are starting to have machines use that same search to try to find and research all these new baseline SaaS features that we're starting to expect. In those companies, generally, they adopt us alongside their existing lexical search and then over time shift more and more search workloads over to turbopuffer, including some of the net new within the business, but also incorporating some of them and then querying both. Over time, you would like them to move everything to turbopuffer.

**Bhavik Nagda** [9:43]:

Ankur, are you seeing that? Companies are also benchmarking, call it latency recall of the vector systems that they're using in your product. What's the entry point to Braintrust?

**Ankur Goyal** [9:50]:

Yeah, so I actually haven't seen people run performance benchmarks, like speed benchmarks, of different vector databases in their evals. I think the reason is that you don't really need evals to do that. If you have a set of cases or the shape of data that you want to test the performance on, you can test it once or twice and get a reasonable measure. But if the words inside of a particular paragraph of text change, it's unlikely to affect the speed at which a vector database returns stuff. We do see a lot of people actually eval the quality of search results that they get, and we do see a lot of people find silly concurrency problems where they're serializing vector database calls or something in their waterfall of a trace. There are quite a bit of turbopuffer API calls that get traced in Braintrust one way or another, but honestly, I think most people that we work with that use turbopuffer, when I ask them about it, they say the decision was pretty straightforward and easy.

**Bhavik Nagda** [10:55]:

It's helpful. And even just taking a step back, if I was building an AI agent net new, what's the best time to start creating an eval harness or starting to do tracing, production monitoring, all this stuff that Braintrust provides?

**Ankur Goyal** [11:07]:

Yeah, I would actually say it's on day zero. When we started, we talked to a bunch of companies and we found that the people that were most interested in Braintrust were people that had shipped their product three months ago because you'd have this people tend to have a bit of confirmation bias leading up to shipping a product that they've played with. Let's say you and I are working on a product. We might sit in a corner and play with it until we feel like it's good, and then we're like, "We don't need evals; we were able to improve this thing." Then you ship it, and then Talia starts using it, and she's like, "Wow, this thing sucks." You're like, "What? We were sitting in a corner, and it was fine." No, it actually sucks. You're like, "Why does it suck?" And she's like, "Why do I have to tell you?" That is usually the point at which people realize that they need evals. However, the people like that start doing evals realize that if you're not doing evals while you're actually specking out or thinking about a project, then you're going to waste a lot of time. Usually, after people get bitten by the eval bug, they start doing evals as they're actually prototyping things. A large part of that is if you use evals from day zero, you can build a prototype that kind of sucks, but you have a feedback loop built into your development process that lets you improve the quality of the product very quickly. If we were using the metaphor of you and I sitting in a corner, we'd have to sit in the corner for a lot less time to be able to ship something than if we weren't using evals.

**Bhavik Nagda** [12:34]:

That sounds cool. I just wanted to flag for the group, if anyone has any open questions, please do add them into Zoom, and we'll make sure to answer them as we go.

**Ankur Goyal** [12:43]:

I Wanted to add to what Simon was saying. You mentioned there's two things that have to change quite a bit for a new database to be relevant. At Braintrust, even though we're not selling a database, we actually build our own database called Brainstore. Simon and I talk a lot about this all the time. It's also built to run directly on object storage, and it literally wasn't possible until S3 released compare and swap, for example. A lot of that stuff is very relevant, but I actually think there's one more thing that's quite different in AI, and I think this is especially true for us—maybe it's a little bit less true for search—but in traditional observability, Prometheus-flavored observability, the information that you track tends not to be tied to PII. The logs that are spit out of your web server or the CPU metrics that you're tracking from all of your containers or whatever, if that leaks, it's not the end of the world. It's obviously not good, but it's not the end of the world. If your web server logs are spitting out PII, then that's usually something like if Simon discovered someone doing that at Shopify, that's usually something that would be considered a pretty serious bug and fixed quite quickly. In AI, the interesting thing is that what you're observing is actually people's raw interactions with LLMs, and the information that you're observing actually inherently has a lot of PII in it. The scrutiny that people have applied to Braintrust from a security standpoint is an order of magnitude higher than the scrutiny that was applied to observability products in the previous generation. That's one of the other benefits of using object storage. We make it very easy for customers—we have a Fortune 10 customer, for example, using Braintrust. We make it super easy for them to run Braintrust inside of their own cloud environment. Part of the reason that's practical for them to do is that Brainstore stores all of its state directly on object storage. I think that is something that is very different than the previous generation of observability systems.

**Simon Eskildsen** [14:54]:

I think there's actually a reason number three and a reason number four in the "why now" of the databases, and you're touching on number three. I just usually simplify it into one and two. The way that I often talk about three is that there's a new deployment model. In 2015, I was working on migrating all of Shopify to the cloud. A lot of the frontier companies didn't go into the cloud until the late 2010s. Now everyone is running in the cloud; even most of the enterprise is living in the cloud. It makes it a lot easier to ship things into people's VPCs without ending up as an on-prem company very early, which just becomes you building a support team very quickly. I think the new deployment model is one, and then I think that I would go a step further here and also say that this actually allows even more deployment models than just BYOC, where this is easy operationally inside of another customer's cloud. I think this is one of the most underappreciated things about this architecture, but also one of the most important that we see when we're outselling, which is that with this architecture, if you guarantee that everything is in object storage, it means that you can use your customer's key to encrypt every single byte of data. turbopuffer exposes this. You can send an encryption key that's managed inside of your cloud to turbopuffer, which is logically the same, even though it exists in our bucket, as if it exists in your bucket. This is fantastic for enterprise because it means that IT gets all the governance controls that they need to have to be able to shut down any data access, but without any of the operational pain of running it even in their own cloud, where you want to lock it down as much as possible. They can go a step further. A lot of our customers in SaaS do this, where they use their customer's key to encrypt the data in the turbopuffer bucket. Not even our customer is able to see their key; it's passed all the way through, and that customer has access to it. We see some of our more advanced customers in their BYOC deployment, which is probably very similar to what you're doing, Ankur, store all of the data inside of a bucket, and then turbopuffer just has access to a particular prefix to run inside of that bucket. All of the data is in there, and it gives the customer this warm fuzzy feeling of knowing that all of the data is in their control, either with encryption key or in terms of coverage case, it could even be in their bucket.

**Ankur Goyal** [17:08]:

When it's the customer-managed encryption key, do you search over the encrypted data directly, or do you decrypt it in memory and then search?

**Simon Eskildsen** [17:17]:

You have to use the key; then we get it into memory, and then you have to accept some TTLs on disk cache and things like that.

**Ankur Goyal** [17:20]:

There's no homomorphic vector encryption yet.

**Simon Eskildsen** [17:24]:

That stuff doesn't really help you very much. It just makes it more complicated if you break into one namespace, but you can do it. You can do these rotations. It just makes the attack take longer. It doesn't fundamentally change the shape of it. The fourth thing that is very nice if you're building a database company is to get a lot of data very quickly.

**Bhavik Nagda** [17:55]:

A lot of traditional database systems have been built on NVMe non-volatile memory. They haven't separated compute and storage in the same way. Can either Ankur or Simon, whoever wants to jump in, just outline the sort of broad scope evolution that we're seeing before we dive into specifics around databases?

**Ankur Goyal** [18:15]:

Yeah, there's not a lot of database systems that are actually built on NVMe. The problem is that if you rewind two or three generations ago—like back when I was working on databases at MemSQL in the ancient days—people were just starting to get access to SSDs regularly on hardware, like on-prem hardware. Maybe back in those days, Simon, you were starting to get SSDs and IO data center.

**Simon Eskildsen** [18:38]:

Mm-hmm.

**Ankur Goyal** [18:39]:

Fusion IO, that's good stuff. But most people can't even afford that. Some database systems tried to build some fancy algorithmic support for these SSDs, but pretty much no one did because SSDs are really bad at random writes, and that is the main problem when you're building an OLTP system. Now fast forward a little bit, and then people got infatuated with the cloud. For a long time, there was no SSD support in the cloud. When it finally came, as these NVMe disks arrived, they were not durable beyond the lifetime of an instance. All of the stuff that people were working on before assumed that the NVMe would survive and be useful as long-term durable storage, and that is still not the case today. I think that's a key difference. I don't know of any commercial OLTP system that is actually built with the assumption that it can use volatile NVMe. Non-volatile is not the right word; non-durable NVMe. I think that's the big difference.

**Simon Eskildsen** [19:45]:

PlanetScale is, but...

**Ankur Goyal** [19:48]:

There we go.

**Simon Eskildsen** [19:48]:

They sold it from the...

**Ankur Goyal** [19:49]:

But I would consider them in the latest generation.

**Simon Eskildsen** [19:53]:

Sure, I think there's also two parts of it, right? There's the operational side, which PlanetScale is obviously exceptional at. Their Kubernetes operator can handle these circumstances because what you can end up with is like three NVMe instances are gone, and then all your data is gone. You have to have an enormous amount of trust in your operations to be able to get onto these NVMe instances. We can talk about the software side for a second. Ankur kind of alluded to the random write piece, which makes OLTP incredibly difficult, at least any B-tree-based OLTP, which Postgres and MySQL are. You need some LSM, and only those are also only starting to mature. I do think that the storage engine that takes advantage of NVMe is also fundamentally different than the ones that have been written in the past. MySQL and Postgres were both written for HDDs so many generations ago, and even SSDs are different, and then NVMe SSDs are different again in the trade-offs you make with memory. The nice thing is that the storage architecture that is required to take full advantage of OPEX storage and NVMe is more or less the same because what you need to do to take advantage of NVMe on the read path is that you need to do a lot of outstanding concurrent requests to the disk in as few round trips as possible because you can't escape the, say, 100 microseconds of random read latency that you have to an NVMe SSD. In the same way that every time you go to S3, you have a P90 access time of around 100-200 milliseconds, depending on the region and so on. You can't escape that, and it's sort of fundamentally the same problem where you can max out the network NIC to S3, and you can max out the NVMe port to the disk, but you have to do an enormous amount of parallel requests in every round trip and minimize the number of round trips. That's not how the storage engines that were built in the 90s or the 2000s or even the 2010s were built with that kind of round trip sensitivity in mind. S3 really forced our hand in making sure that we only do three round trips for everything. The storage engine is capable of doing good cold read latencies on S3 by minimizing round trips and maximizing concurrency. That just also happens to be phenomenal for a disk.

**Bhavik Nagda** [22:44]:

No, that's helpful. Just to understand that better, since you're now using object storage and you want to achieve some degree of data locality, Simon, my understanding is that turbopuffer is best fit for customers that have natural sharding in their data. Can you talk about that a bit?

**Simon Eskildsen** [23:04]:

Yeah, I think that a startup's only mode is focus. Our focus in the beginning was any large multi-tenancy workloads where the P100 tenant was not particularly large, so the individual shards could not be that large. That's not the case anymore. Now we can do shards that are into terabytes without any issues. But in the beginning, you're right. We took advantage of the fact that the largest code base in the world, I don't know, something like LLVM or Linux or something like that, is still not that large. Even for Notion with their workspaces, the largest Notion customer was not that large. So we focused on getting very good at handling a very large amount of shards. Now the plan has always been to get very good at handling very large shards because then you have other customers that have extremely large shards. Maybe their biggest customer has a billion or maybe even tens of billions of documents they want to search simultaneously, and then you want to get good at that. Every database shards; it's just who manages for you and when does it happen.

**Ankur Goyal** [24:05]:

Right.

**Simon Eskildsen** [24:05]:

It's very funny.

**Ankur Goyal** [24:05]:

We have the exact opposite problem. Please keep going, but yeah.

**Simon Eskildsen** [24:11]:

What we're working on now are namespaces that are around half a terabyte, so that works fine now in turbopuffer. You want to max out at probably half a terabyte to a terabyte of shard sizes, but you want to make these shards as large as possible because fundamentally when you're doing search or any type of database lookup, it's like n times log n, where it's not actually log n for a vector lookup unless—let's pretend it is. Then the M's, you want that to be as high as possible because it grows logarithmically, but you want the M to be as small as possible to do as few of those operations as possible. For reference, in Elasticsearch, when I ran that in production at Shopify, you go for a shard size of somewhere between 30 and 50 gigabytes—an incredibly small size. The end log n is very large, which means you're spending an enormous amount of CPU cycles doing that search. Whereas in turbopuffer, our shard sizes are trying to get up to a terabyte, right? Successfully, and then at some point, you have to spread that out into multiple machines and build a sharding management layer on top of that, which will be there in future versions of turbopuffer. Fundamentally, you can just do ID modulus n, which is what a lot of their customers are doing. The short answer is yes, in the first version of turbopuffer, we only supported small shards, but now we can do state-of-the-art shard sizes.

**Ankur Goyal** [25:30]:

Yeah, it's quite interesting because we have, if you use one of those customers like Notion as an example, all of the logs across their users are coming into one Braintrust collection, and the task is actually to look at the information across all of them. We don't have sharding based on a collection, but we do many of the same tricks by using time as a partitioning key.

**Talia Goldberg** [25:58]:

With that, I think it would be awesome if we could go to a brief demo from both of you. Maybe we'll start with you, Ankur, and talking through a bit of a product demo. I think one thing that we heard as folks were signing up for this session is that there are some questions around how to structure evaluations for multi-step agent workflows where one failure will cascade through an entire process—just things around best practices. As you go through and do the demo, you can just talk a little bit about both of those things.

**Ankur Goyal** [26:32]:

Yes, I'm very happy to. Give me just one second.

**Talia Goldberg** [26:34]:

We'll see if the demo gods are with us.

**Ankur Goyal** [26:36]:

I think they are. I just want to pull up the right project. Okay, great. Great. So we have an agent built into Braintrust. Before I talk about evaluating agents, I'll just show you a little bit about that agent because then we're going to look at the logs for that agent and a little bit about how we evaluate it. Here is an example of a question I ran. I can do a fresh one: "Who uses this feature the most?" Similar to things like Cursor or Quadcode, this uses an LLM with a bunch of tools available to it, in this case to analyze the data in my logs. It will run a bunch of searches over the logs, which take advantage of a lot of the things Simon was talking about. For example, in Braintrust, if you run a search and we read from object storage, it might be slow the first time, but then all that data is cached in NVMe, and the second search is much faster. This thing will take advantage of that, and it's going to try to run a bunch of searches and then figure out who uses the product the most. Now behind the scenes as that runs, it's actually generating traces that look like this. This is like a classic agentic trace in something like Braintrust. Here's a system prompt with some instructions and then the user's question and then a bunch of tool calls, and you can see them interleaved. If you can understand the timing of each of these calls, you can also do stuff like understand the conversation in a more chat-like experience. You can see, "Okay, to that," and this is a lot of the debugging that people do when they're actually playing around with something like Braintrust. Now, the other thing you can do that is quite cool and something Simon helped me figure out when we were first working on Braintrust is search. I can search for something like my email address, and you'll see that it will complete very quickly, and it will also actually update all of these aggregates as well. That is again something that takes advantage of some of those properties Simon was talking about. A lot of the data to calculate one aggregate is similar to the data that calculates another aggregate, so once it arrives on NVMe, it's quite easy to actually run a bunch of redundant or similar calculations over the data very quickly. I think again it allows us to build a really cool user experience. The last thing I'll show you is how this actually translates to evals. I have a project, and this is a little bit stale; I should probably rerun this. When we were first shipping this feature, we actually ran a bunch of model comparisons. This is a pretty popular thing that people do in Braintrust. They take use cases like the one that I just showed you. If we click into this, you'll see that this experiment actually looks very similar—like these logs look very similar to what we were looking at before. You can do tracing in production, but you can also do tracing when you're evaluating to get all the same debug ability. If we go here, you'll see we can actually evaluate different models side by side and understand the trade-offs and stuff between them. This is the kind of stuff that I think is really powerful. There's a lot we can talk about with questions if people are curious about how to specifically evaluate agents. The one thing I'll just quickly call out is I think it's super important, in addition to running a bunch of really good scores, to also track a bunch of metrics that help you understand the relationship between LLMs and tools. The most useful thing I find is looking at stuff like tool errors and trying to see if I use a different model or if I change the prompt or something about how it runs, do I suddenly get a lot more latency in my LLM calls and I suddenly get a lot more tool errors—stuff like that.

**Bhavik Nagda** [30:16]:

Ankur, if I have a production system set up and I come in and say I can see the logs and 95% of them are great, but 5% of them are wrong, how should I start to prioritize what to work on or what to focus on?

**Ankur Goyal** [30:31]:

Yeah, I think one super simple thing you can do is capture user feedback, and Braintrust allows you to do that quite easily. You'll see here there are some score columns, and you can do stuff like filter. Let's see. Sorry, I think I need to refresh the page. Yeah, we can filter and say, "Okay, I want to find all of the examples where there's feedback." This is a pretty quick and simple way to just surface things that may be interesting. If you are early in your application development lifecycle, you probably don't have that many interesting things. Right here, I have six, so I could probably look at all six of these and look at the thread view and try to understand what actually happened here and why was this a good or bad experience and then turn that into test cases to use in an eval. If you find that you have too many, then you really have two options. The first thing that people think about, which I think is a cool thing to do but not often that useful, is to try to come up with fancier scoring mechanisms. Maybe you have a thumbs up and thumbs down, and you have 10,000 of those, and you come up with another scoring method, for example, using an LLM to look at the outputs in addition to that and filter that down further. The simple thing to do, which is what I honestly recommend in a lot of cases, is just look at the first 10 of those 10,000, and you'll probably find something interesting. Every time you go to the logs page, if you find one interesting novel case, then it's ROI positive. You don't really need to think about it or overthink it too much. As long as it is relatively time-efficient for you to find novel interesting things that you haven't seen before, then things are good. If it's not, then you should invest in more scoring to help you narrow things down further.

**Bhavik Nagda** [32:30]:

Are there any common dark patterns or common mistakes that you see people make either setting up eval harnesses or trying to close this loop?

**Ankur Goyal** [32:39]:

Yeah, I think a few things. The first is that people will only do online evals or only do offline evals. I think you should think about your job building AI software as to build a feedback loop because you can't predict what a prompt is going to output. You need a feedback loop from what people actually experience to what you're developing. To build an effective feedback loop, you need both offline evals and online evals. Some people don't do that. Another thing that we see people do is only trace LLM calls. It's very easy to do that; you can use our libraries or there's some proxies and stuff that will let you do that. You get some visibility into what's happening by just looking at the LLM calls, but you get significantly more if you can capture the interleaving execution of LLM calls and tool calls. I think it's very important to put in a little bit of work and get traces that reflect the information that you actually want to see. The third thing is if you're very lazy about scorers, the anti-pattern that we tend to hear is, "Do you have a pre-built scorer for hallucination?" That's probably one of the most common questions that we get. If you're lazy about that, then you're just not going to get good results. The reason is that I think scoring is essentially like the AI evolution of writing a PRD. It's your opportunity to come up with the criteria that are very specific to your use case that, if met, will result in a good user experience. If you're lazy about that, just like you're lazy about writing a spec or writing a PRD, you're going to end up with a crappy user experience that regresses to the mean. On the other hand, if you use it as an opportunity to create differentiation for your product and really think about how to capture the attributes of an experience that you think represent a good outcome for the user, then you'll get a really good experience. We really encourage people to create and customize their own scoring functions to try to find stuff that is relevant to them.

**Bhavik Nagda** [34:40]:

What's a good example of that?

**Ankur Goyal** [34:42]:

So we have two customers that are very different—Vercel and Stripe. Hallucination for Vercel means something very different than a hallucination for Stripe. If you try to use the same logic to figure out whether something is a hallucination, I think you might, in the case of Stripe, miss things that are actually really important that you don't make up. In the case of Vercel, you might not allow the model to be as creative or open-minded, if you will, about the code it generates to help someone achieve what they're trying to do. Actually trying to think about what that means in the context of Vercel, for example, you can statically analyze code, and if it references a library that is not imported or doesn't exist, that is a very verifiable form of hallucination that is just completely irrelevant for a customer support bot and a financial company.

**Bhavik Nagda** [35:32]:

That's helpful. Maybe one more question, and then we can chat a bit about the demo for turbopuffer. One of the biggest gains of the last year or so with regards to these agents has been tool calling and their integrations with third-party existing systems. Now imagine when you're calling tools with an agent, you could get back an image, you get back a PDF, you might get back text, increasingly maybe videos and multimedia. How far do you anticipate Braintrust will go in terms of storing that and creating that trace?

**Ankur Goyal** [36:04]:

Oh yeah, we already support that. We have a feature called attachments. The way to think about attachments is like in a traditional database, if you have a VAR char or a variable string, it doesn't store it directly in the B-tree; it stores a pointer to it, and that lives in a cheaper and simpler form of storage with some performance trade-offs. We do exactly the same thing for multimedia. You can upload attachments to Braintrust, and they get stored directly on object storage as blobs and referenced inside of the data that's actually indexed. It's very cheap to do that as a result, and the trade-off is that you can't search the basic C4 content in an inverted index, which no one cares about. It actually works quite well.

**Simon Eskildsen** [36:48]:

That's super helpful, and you mentioned you use turbopuffer insertion across Braintrust docs within this platform natively.

**Ankur Goyal** [36:55]:

So we are about to release a feature that uses turbopuffer, and that feature lets you search the Braintrust documentation automatically. The word doc is probably the most overridden term that Simon and I could refer to in this conversation because it means many different things. We unfortunately don't currently use turbopuffer to power the search experience and stuff inside of Braintrust, but Simon and I are always scheming about interesting ways we can collaborate.

**Bhavik Nagda** [37:28]:

Well done. Awesome. Maybe with that, I know Simon, you wanted to introduce the demo TurboGraph and show the capabilities of turbopuffer.

**Simon Eskildsen** [37:34]:

Yeah, for sure. Yeah, it's a database, so I don't have a nice UI. Actually, I do have a UI. I can show the UI and then I'll show you TurboGraph. Unfortunately, we are a lot better at Rust than we are at React, so bear with me. Maybe if someone in attendance wants to come help us with the dashboard, they should come join us. This is what turbopuffer looks like in the dashboard. It's very simple. This is our test account. You can see here it has almost 5 billion documents, and the invoice is only $700. Now, we don't charge ourselves, but it gives you an idea of the kind of scale you can get to with good economics on turbopuffer. Very simply, it just shows you what's going on. You can see the namespaces that you have here. When we test, we just create a namespace for every single one of the unit tests that we run, so there's a lot of them. I think there are tens of thousands. Some of our customers have millions, but it's a very simple UI here. I will now show you TurboGraph. TurboGraph was just something that I've been hacking on when I have time. I think Ankur has more time to code than me. I don't know how; I don't think Ankur sleeps very much.

**Ankur Goyal** [38:46]:

I can tell you my story after this.

**Simon Eskildsen** [38:48]:

Yeah, please. Turbogrep is basically just like ripgrep aspirationally in the limit one day. Instead of searching with regexes and full-text search, it embeds the entire code base and then searches over the embeddings. What that allows you to do is you can do a search like "Upsert to Vector Store," and it will return the actual function that's doing that—in this case, like "write batch," which is doing that. Inside of my editor, I can do "upsert to Vector Store," right? It doesn't know. It will have to find out that it's turbopuffer and that it's "write" instead of "batch," and it just finds the function. I use this all the time because I honestly read a lot more code than I write code, and being able to do a semantic search like this is extremely useful. You could also do something like "How does it do a fast search while indexing?" to try to lead you to the right thing, and it needs me this function called "speculate search" because it's creating the embedding itself while it's chunking the whole code base to keep it up to date. I can go read this from as I map my own vocabulary onto it. I could do something like ASCII emoji for puffer fish, and it will take me to the progress bar here, which is like an ASCII puffer face that's inflating and deflating as you're doing things. It's a very simple demo, but a lot of our customers do this sort of fundamentally connecting data to AI, whether it's code or documents or PDFs or some kind of unstructured data. You can see here if we reset this namespace, then what it will do is that it will chunk the whole namespace, then it will create, then it will find the closest region, which I'm currently closest to the Toronto region for turbopuffer, and then it will create an embedding. This demo right now uses Voyage and creates the embedding, and then it does the TPuff search, and it took about 25 milliseconds once everything was written in and then served the search result. If you do this, it will chunk the whole code base again and then rerun it, so it's very simple. We wanted to have something simple out there for our customers to use, and also it's a very useful tool for me. This will work also on much larger code bases. Actually, maybe we can use that. Do I have Rails or something? I don't know if this is going to work. Split a string. I don't know. Let's see here. Let me do this for both. You can see here it's chunking the entire code base, and then there are 53,000 chunks, and it starts to just pump these into whatever H100 Voyager is running to create the embeddings. It's a bit slow because I'm using a pretty slow model, and I don't have all the rate limits. turbopuffer can do about—could do up to 10,000 or more writes per second, so this is currently limited by the Voyage model or my uplink, one of the two. It's just going through the code base, and eventually, once this finishes in about another minute, it will return to query. But yeah, that's it. That's my little demo.

**Bhavik Nagda** [41:36]:

Yeah, Simon, once you've chosen turbopuffer for your performant database system, there are a lot of levers you have—the chunk size, the embedding model, the re-ranking model—to improve the search quality. Where do you recommend people start, and what has the sort of highest effort-to-payoff ratio?

**Simon Eskildsen** [41:54]:

My general answer here is find an embedding model that's fast because we've seen customers who have embedded tens of billions of documents with an embedding model, and then they get great throughput—like more than the 500 a second here I'm getting on my little probably trial account with Voyage—and they pay potentially hundreds of thousands of dollars to embed all of their billions and billions of documents, and then the query latency is 300 milliseconds. You want to choose a fast embedding model, and that can vary a lot because you might run all of your workloads in Frankfurt, and whatever embedding provider you're using might not give you any choice over the region and rank it all to Oregon. Now, no matter what, you're paying a 200-millisecond penalty to get that embedding unless you have more control. Getting finally finished, it just takes a lot of time. We recommend spending a little bit of time. You can use any code agent to very quickly just find the one that's fastest for you and run it. The other thing is that we recommend that people start with something very simple. Just start with a simple embedding-based search on a small use case and then get it in production. Don't try to do anything fancy until you have the evals in place because if you start to do something complicated before you have the evals, you end up in this space where you build a complicated system, but you have no idea where in the complexity the value is. You want to be able to do that to start building your intuition. What I generally say is stand up the simplest possible thing that passes vibes and then try to get the evals going as quickly as possible. Then it's about creating the evals. Creating the evals is like you have to create a bunch of evals, make them, and then the people on your team have to start creating evals. You should probably also recruit your cousin to create evals, but you need a lot of evals. When I was working on search at Shopify, we spun up massive teams of just people that saw it and created these evals. Today, we have other means of doing that, but it's a very traditional way of doing it, and everyone knows that's the best way to get good search results. Then you can start layering on complexity, right? You can do a query rewriting layer where you start to rewrite queries. You can use lexical search. You can use multiple embedding searches as part of the same one. You can start to use late chunking, late interaction. You can train your own re-ranker. You can try different re-ranking models, but do not introduce complexity into the search pipeline until you have the evals in place because otherwise, you will get addicted to complexity that you have no idea if it adds any value.

**Bhavik Nagda** [44:13]:

That's helpful. The other trend we've seen is like as we move to these agents from multi-turn LLM systems doing searches, they might—an agent might first try a semantic search, then try the traditional search, and it seems like turbopuffer offers hybrid search for those types of use cases.

**Simon Eskildsen** [44:37]:

Yeah, that's right. I think we find that there is—let's say that an embedding model doesn't know what turbopuffer is. It might start returning like "fast fish." I don't know if like a sailfish is the fastest fish, and that's not helpful, right? Because it doesn't know what it actually is. Whereas a lexical search model will know exactly what it is because it has no idea; it just sees that it's a string. Embedding models are very good at turning strings into things, but if it doesn't know what the thing is—which is often the case in, say, you have a Notion workspace and you're using some internal wording that the embedding model could never know what actually means—then the lexical search comes in really handy. It's great to use both, and we find that most of our customers have a lot of success with that, especially if you're powering some kind of command case search, right? Where you want to search for "si," and the embedding model is going to think that that's "c," and that's something agreeable, and yes, right? It's Spanish for "yes," but really what you want is just to find a document that starts with Simon, right? These things have to play in concert. Generally, for search applications, it's pretty idiosyncratic what works well. We try to give opinions about what we've seen work well on average, but you need to have the evals in place to go from there.

**Talia Goldberg** [45:51]:

This is awesome. By the way, thank you. I can see I love that you both are basically selling each other's products throughout this entire process. It goes hand in hand.

**Ankur Goyal** [46:02]:

I think we've seen a lot of customers actually be successful with both products together, which is pretty cool and not a super common thing, at least in my experience.

**Talia Goldberg** [46:13]:

We've seen the same, which is why we were so excited to have both of you on together as part of this because we knew that it would lead to great back and forth and that there was so much overlap from just what we've seen even across our portfolio and customers. I know we only have a couple of minutes left. I would love to wrap on just hearing a little bit from both of you on your own stacks—like your own setup, whether it's your own coding setup or tools internally that you all use—anything that you can share that you think would be interesting. To the broader group, we found a lot of the show and tell to provide a lot of value, and y'all are experts and have good noses for what's the latest and greatest at the cutting edge, so would love to hear that.

**Ankur Goyal** [46:55]:

I can go first. I think the main thing that has really changed for me is moving from synchronous programming, which I also don't have the bandwidth to do a whole lot of right now, except between the hours of 9 p.m. and 12 a.m., to asynchronous programming. I think asynchronous programming is quite powerful, and I like to think of it as like for people that are professional programmers, it's like what vibe coding does for people that are maybe programming not professionally. Asynchronous programming is basically working with a really powerful coding agent or multiple coding agents at once to execute something that is very difficult. It requires thinking a lot about testing and evals, for example, so that you can let the agent iterate really effectively. It requires reading a lot of code. It requires thinking about scope. You can burn out an agent very quickly by giving it too much scope, but if you figure out the right abstraction to let it work in, it can be very effective. I've spent a lot of time over the past, let's say, six months or so trying to just personally hone that skill. I don't think I'm like the number one world's best asynchronous programmer, but I think I've improved quite a bit, and I've seen some people who are extraordinarily good at it, so I'm just generally quite excited about this trend. It'll be interesting to see how truly productive great programmers who embrace it can be.

**Talia Goldberg** [48:22]:

We've seen some pretty wild demos and just even things on what people are doing that have really embraced this on Twitter and even are orienting their sleep schedules around stop agents.

**Simon Eskildsen** [48:33]:

This is the return of the Uberman sleep schedule where you'll sleep one hour and then you're up for an hour and a half or whatever it is. No, it's less than that. It's like 20 minutes every two or three hours when the agents wake up. I think I'm leaning somewhere very similar to Ankur. I also don't have a lot of time for the synchronous programming and finding the one to two hours of focus that it takes to get some of these bigger chunks done. So it will be a lot of terminal agents. I'm starting to use the background agents a lot more to do various small things. Like yesterday, I was putting up another case study, and it's just, "Okay, here's the Markdown file," and then I use this program called WhisperFlow so I can just yap for a minute about exactly what needs to get done. Generally, the agents are very good at that when you can just speak instead of typing, which is a bit of a bottleneck for me. Then I think there was about a six-month period where you have to use the coding IDs, and if you didn't, that was a massive performance loss. During that time, I started having wrist pain again, which was why I switched to NeoVim in the first place. So I'm back to reading and reviewing all of my code in Vim and then having a lot of the background agents and the terminal agents doing the work. The bottleneck now very much is reading the code and coming up with good instructions, and that's what I've been doing for the past many years. That setup is very well set up. In the event, I have a script called review where you just pass it a GitHub URL, and it just gets the whole diff into Vim and like in buffers and with chunks in the gutter, and that's been very helpful. Now that you can just...

**Ankur Goyal** [50:02]:

Can you send me the script?

**Simon Eskildsen** [50:03]:

The agent? Yeah, I can send you the script. Yeah, it's open source; it's in my dot files. It's like a teasingly coded. This is like a pre-LLM, so you know...

**Ankur Goyal** [50:13]:

I use NeoVim as well. Actually, I went through exactly the same thing, although I started using an IDE when Figma acquired us just to learn about Copilot. The last act of my IDE was actually writing my new NeoVim config file, which felt very nice. It was like a nice piece of closure. There are actually a bunch of cool things in IDEs now. The LSPs have gotten a lot better, and you can recreate a lot of the IDE features in NeoVim, I think, quite nicely now.

**Simon Eskildsen** [50:45]:

Yeah, and there are some distributions that make that really easy. Turbogrep, just to sell my little open-source project here, is very helpful for reading code in unfamiliar code bases, which I also spend a lot of time doing—like reading dependencies, things like that—which takes a while to map the vocabulary that you have in your head of how the thing works to how it actually works.

**Talia Goldberg** [51:03]:

That's very cool. All right, team. I think we're at the hour. Thank you guys so much for spending the time. This was great for everyone on the call and following along online. We'll follow up with ways to contact and learn more about Braintrust and turbopuffer. Check out their sites; they're also hiring. Enjoy! But thank you guys so much. This was awesome.

**Ankur Goyal** [51:29]:

Thank you for having us.

**Simon Eskildsen** [51:31]:

All right. Thanks a lot.
