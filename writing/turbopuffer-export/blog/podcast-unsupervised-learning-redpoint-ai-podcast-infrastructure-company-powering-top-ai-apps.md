# The infrastructure company powering the top AI apps

July 22, 2025•Unsupervised Learning

[Video 3](https://www.youtube.com/watch?v=LsvBqkF8Jvs)

## Transcript

**Jacob Effron** [0:00]:

turbopuffer is an incredibly fast-growing vector database and search engine, powering some of the leading AI applications out there, like Cursor, Notion, Linear, and more. Today on Unsupervised Learning, we sat down with their CEO, Simon Eskildsen, who had a fascinating career prior to turbopuffer, spending a decade at Shopify working on their hardest infrastructure challenges. We touched on a lot of things in today's conversation, including how builders should think about using different databases. We talked about the evolution of the vector database space over the last few years and why now is a particularly exciting moment. We hit on Simon's takes on the general AI infrastructure landscape and what areas will persist. And we also talked about what he's learned from building so close to the forefront of AI apps today. This is an episode I think folks are really going to enjoy. Without further ado, here's Simon. Well, Simon, thanks so much for coming on the podcast. Really excited to have you.

**Simon Eskildsen** [0:41]:

Yeah, thanks for having me.

**Jacob Effron** [0:43]:

And get to do it in Ottawa. It's a treat to take the show on the road.

**Simon Eskildsen** [0:46]:

Yeah, thanks for coming.

**Jacob Effron** [0:47]:

I was thinking about a few ways we could start. One is it feels like in building AI applications, everyone is incredibly focused on connecting their business-specific context to their applications. And I feel like throughout the last few years, there's been a bunch of different ways folks have started to think about doing this. One has been, let's just stuff everything we can into a context window as they get bigger and bigger. Another has been using kind of net new databases. Talk about the origin of what made you convinced there was a need for a new search paradigm here.

**Simon Eskildsen** [1:17]:

Yeah, so I think when I started working on turbopuffer, the context windows were very small, and so that really got the initial wind in the sails of the first few vector indexes, right? The context windows were maybe 8k, very small, and so a lot of the applications that started needed retrieval-augmented generation (RAG) basically right away. Like most, even most articles are larger than 8k tokens, so that really got early wind in the sails. But then the context windows got very large very quickly, right? They came up with these windowed mechanisms and stuff like that to try to make the context more useful. It's still quadratic; there's still a lot of tricks. It's not perfect recall at the end of the day. There are very few datasets that actually do any kind of reasonable question answering on something the size of a Harry Potter book, so it's actually very difficult to train a good model to do this. But either way, that's sort of got the initial wind in the sails. But then it felt like the wind kind of got out of the sails a little bit because the context windows got really large. And the first few applications that were coming online were able to just abuse and stuff the context windows and run for product-market fit (PMF). And that's exactly what I would have done too. But what we're seeing now is that even with a million context or 10 million context, let alone the latency, companies want to connect all of their data to LLMs, right? That is the new workload that we see today. And in order to do that, you often have more than a million tokens; you have tens of millions of tokens; you have tokens that have permissions attached to them. You want very high recall, which even in a very large context window, you can't necessarily guarantee that it can be there. Again, back to it's very difficult to train a model on this kind of thing, right? So I think now what we see is that companies want to search large amounts of data as they have found PMF with the large context windows, but they demand economics that make sense for them. What counts for them is that the per-user economics of storing all of this data have to make sense for the value that they're delivering on the other end. And on a traditional storage architecture, where you're storing every byte on three disks, the per-gigabyte costs end up like north of a dollar, right? Per logical gigabyte, and we can do it at a much, much lower price with object storage and with a set of trade-offs that make really good sense for search that allows searching on, you know, not a million tokens but hundreds of millions or billions of tokens for a particular user or application. I have a little acronym that I throw around when people, when we talk about content.

**Jacob Effron** [3:55]:

We love acronyms.

**Simon Eskildsen** [3:55]:

It goes like this for databases.

**Jacob Effron** [3:56]:

Yeah.

**Simon Eskildsen** [3:57]:

I call it SCRAP, and it's basically like a couple of different, and I just use this when I, you know, otherwise I forget something. The first S stands for scale, right? So it's a particular point you will outgrow the size of the window. Even if the context window gets to 10 million or 100 million, right? People are just going to demand more; that's what tends to happen with computers, right? But there is some sense where, okay, if I'm doing an analytical query, should I load a billion rows into a quadratic attention window, or should we have some auxiliary system, right? It feels like even an AGI-level model would build a system to do some of this stuff right on what's already there. So that's the first one. The second one is C, so that's cost. The cost of executing a very large context window is substantial. You have to store that in VRAM, and VRAM is one of the hottest commodities on Earth right now. It makes a lot of sense if you're doing a lot of queries against something, right? Some places will make sense to just have everything in a context window and just execute against it, and the economics of VRAM make that work. But in a lot of cases, you can't afford to keep all of this in VRAM, and you can't even really afford to keep it in DRAM, let alone even on disk, right? Which is why this architecture of storing things on object storage can make sense. The next one is recall. So recall in a very large context window can be difficult.

**Jacob Effron** [5:17]:

Yeah.

**Simon Eskildsen** [5:17]:

There are a lot of benchmarks that are sort of needle in the haystack, and they do fine on that because it's easy to train. But actually having datasets that are good at reasoning over very large contexts, large corpora, and just filling an answer, there's not a lot of them, at least that I know of. Maybe they're behind a paywall, so I don't have access to them, but there's not a lot of datasets for that, and you have to play a lot of tricks to make this work. The next one is ACLs, right? I don't think anyone trusts the context window enough that you can stuff in and say, well, you know, Jordan and Jacob have access to this document, but Simon doesn't, right? Can you just like, you know, pinky promise that if Simon is the one asking, that you won't do it? There will probably be a point where we trust them enough, but at present we don't, right? We sort of like put some UIDs on a document and some UIDs in the query and make sure there's at least, you know, one overlap in the set intersect to make sure that the user has access to it. And then the last one is performance, right? It's difficult on these frontier models to load in a very large context window and get a response in less than a second, which is what an engaged consumer would want. So those are some of the reasons that we see among our customers of why they engage with allowing their LLM to access the data in a different way than just putting it all in there. But of course, there's some optimal function between how much goes into context and how much is retrieved into it that's idiosyncratic to the company.

**Jacob Effron** [6:39]:

It feels like so much of this is connecting massive amounts of data, and it feels like in the early days of people looking at some of these net new databases, you know, they're really making up for the fact that the context windows were small. And I remember when we were looking at some of these businesses two years ago, there was just a desire to find like who are the customers that are connecting massive amounts of data. Maybe two years ago there weren't that many, honestly, that were, and now it feels like that is actually starting to change. And you know, you're seeing a bunch of different use cases on top of your platform with folks that are bringing tons and tons of data to actually connect into the models where you need something like this.

**Simon Eskildsen** [7:07]:

I think that's exactly right. In the beginning, the first generation of these databases and most of the research was really around doing vector search in memory. And when you have an 8k context window and you're trying to make up for that with maybe, you know, accessing 32k tokens, the economics are not so bad; you're not really going to feel the pain yet. Of course, we've done inverted indexes, which are what you use for a keyword search on disk for a very long time. But none of this has been tried on something like object storage. But I think that now, yeah, we're seeing that the frontier really is to try to reason over very large datasets. And I think I only develop more conviction as I saw the frontier models start to use so much time querying data, right? They do use a lot of time searching the public web, and increasingly the models also have access to private data. I think every company is going to—there's going to be a baseline expectation that you can fire off some research function, which I think are the agents that people use today that are the most useful, right? Hey, go out and compile a report on this.

**Jordan Segall** [8:24]:

I'm curious on the object storage architecture side. We're big fans of it, by the way. We just released an InfraRed Report and highlighted turbopuffer as an example. What are some of the trade-offs of actually using that architecture, and what are AI workloads that maybe it wouldn't make as much sense for?

**Rashad** [8:38]:

Hey guys, this is Rashad. I'm the producer of Unsupervised Learning. And for once, I'm not here to ask you for a rating of the show, although that is always welcome. But I would love your help with something else today. We're running a short listener survey. It's like three or four questions, and it gives us a little bit of insight into what's resonating and how we can ultimately make the show even more useful to you, our listeners. The link to the survey is in the description of this show. I promise you it takes like two or three minutes, and it's a huge help to us. We're always trying to make the show better, and so this is one way of supporting. And yeah, that's it. Now back to the conversation.

**Jordan Segall** [9:18]:

What are some of the trade-offs of actually using that architecture, and what are AI workloads that maybe it wouldn't make as much sense for?

**Simon Eskildsen** [9:21]:

To back up a second here, I think it's important to take the history lesson of why this is now possible because I think there's a lot of—

**Jordan Segall** [9:29]:

Love why now as VCs; that's a classic.

**Jacob Effron** [9:30]:

Classic.

**Simon Eskildsen** [9:32]:

So I'll entertain that for a second because I think that there's a new storage architecture here, right? Richie and team did it for WarpStream, right? Where we started using this for streaming. The observability providers have done this for a long time. And of course, the OLAP companies that came out in the 2010s have also done this for a while. So what's changed in the past five years that makes something like this possible? So I think the first thing are NVMe SSDs being so good. I think they got good very quickly, and I don't think that any new database engine has fully been built to take advantage of how much bandwidth you can drive from these disks, right? If you go to a cloud and you look at the cost of these NVMe SSDs based on the instance, you can get them for somewhere around five to eight cents per gigabyte, okay? But you can drive often these disks within spitting distance of DRAM bandwidth, right? So you might be able to drive like 10 gigabytes per second to these disks. DRAM on that machine might be 100 gigabytes per second. But they cost two orders of magnitude less, right? A gigabyte of RAM in the cloud costs somewhere around $5, right? It's like two orders of magnitude less in cost, but the bandwidth difference is only 10x. This is a very interesting trade-off, right? And that wasn't really available on the network storage that's always been available in the cloud. So this performance is a very interesting characteristic, and you have to write your storage engine to really take advantage of this, right? io_uring is only something that's come out recently. You can't even use the Linux page cache because it's too slow to keep up with these drives. So these only became GA in AWS and GCP and others in the late 2010s, like I think around mid-2017 on AWS and maybe mid-2016 on GCP, right? There was like one SKU that had these disks. So that's the first thing. The second thing is that S3 only became consistent in December of 2020. This is kind of a mind-blowing fact to me when I learned about it, but what this means was that if you put an object and then you read it immediately afterwards in another request, that you know that what you just wrote is what you're reading back. That only became a core primitive guarantee of S3 in December of 2020. That's a very nice thing to have if you're building a database on object storage. You can live without it, right? Snowflake and others have built big metadata layers so that you can work around this. But it's really nice to have if you want to write a database fast. The third thing that you need is that you need compare-and-swap. And what this basically means is that when you're building a database on top of a file system, right, you have a bunch of metadata of, hey, the most recent version of this is here. And so you need this metadata, and you often have multiple writers that are writing to that metadata. In our case, it would be multiple nodes that might be contending on a piece of metadata. So you need some synchronization primitive. So past database systems that are distributed systems will have a Zookeeper or Raft layer or something like that to contend for this control for multiple writers. But S3 actually only about six months ago, we're recording this in the summer of 2025, released compare-and-swap at re:Invent. Now, every other object storage actually had it prior to that, namely GCP, which we started on for exactly that reason because we had conviction that this was going to become a utility function. Any database that wasn't built this way would feel archaic in five years, but that was the last piece of the puzzle. So NVMe in 2017, S3 becoming consistent in 2020, and then finally compare-and-swap in 2024 allowed you to now build a database where everything is on object storage. And so I felt that you could build a database that had these object storage trade-offs that work very well for search. It doesn't work for everything. So back to your original question, kind of building up to the answer here. What are the trade-offs of building a database on object storage? Well, first, it had to be possible. It's now possible because of these three primitives. And then we have to consider the trade-offs. And so the trade-off is that every time you write, we have to commit to S3. The p90 for such a write, depending on the size, is maybe around 100 to 200 milliseconds. So every time you write, it takes 100 to 200 milliseconds. Now, if I'm writing a checkout system at Shopify scale, then that's too long, right? We can't wait for every transaction to commit that long. But if you're building a search engine, it's usually fine. You update a product, and it takes 100 milliseconds or 200 milliseconds for it to be updated in the search engine. Well, that's an acceptable trade-off in a lot of cases. Fundamentally, this is really the main trade-off. Like everything else, I think in terms of cost, in terms of flexibility, in terms of scale, in terms of simplicity on the system are some of the upside. You could state another downside would be that occasionally you will have a cache miss, and you have to read from object storage, but there's nothing that prevents you from having it always on and hot as you would in a traditional storage architecture, even on multiple nodes, if a node was to go away. The high write latency is really the fundamental trade-off of this kind of architecture.

**Jordan Segall** [14:48]:

And as you think about the implications of that then for AI app builders and when they should reach for turbopuffer and when they shouldn't, like how do you think about the types of things people are trying to do in applications where this trade-off doesn't make sense?

**Simon Eskildsen** [14:58]:

Right? So, I mean, turbopuffer is mainly a search engine, first of all, right? So if you're modeling all of your data and user permissions and all of that, you should probably use a relational database. Nothing's going to make that go away. And the transactional performance of that, the flexibility, and all of the know-how, right? So I'm talking here from a storage engine perspective. Where something like this architecture makes sense for your search engine, right, is that if you have a million vectors and you tag on a vector search extension into your relational database, great. That's what I would do. But if you have tens of millions, hundreds of millions, you have maybe billions or hundreds of billions in the crosshairs. Well, at some point, the somewhat idiosyncratic trade-off of adopting another database and ETLing into it is going to start making sense, right? Because at some point, you can't escape the economics of, well, if you use a Postgres extension, you have to replicate it to three disks. And these disks cost you $0.10 per gigabyte, 50% utilization, all in about $0.60 per gigabyte that you store. You can't really escape the economics of that. And the vectors are large, right? At kilobyte, the tax easily turns into 30 kilobytes of vector data. So where something like turbopuffer makes sense is when you're searching over large quantities of data, right? And I mean, since the beginning of time, people have taken out the full-text search workloads from the relational database. Once it reaches a certain scale, vector search is even more brutal on these transactional engines in terms of both the cost and how much it can overload the box, and these are the reasons why we've taken FTS out for a long time so that would be another bottleneck, right? As an application gains success, you have to start taking out some of these pieces from the relational database before you have to shard it.

**Jordan Segall** [16:47]:

What are some of the most common set of use cases you are seeing people build on top of turbopuffer? I mean, is that some of these deep research-type use cases on top of tons of data or like as varied as the landscape is out there?

**Simon Eskildsen** [16:57]:

I think, I mean, we could go through some of our customers and how they use it. So Cursor was actually the first paying customer on turbopuffer.

**Jordan Segall** [17:07]:

Pretty good first customer.

**Simon Eskildsen** [17:07]:

Yeah, seriously. And they've been wonderful to work with and true design partners in every sense of the word. And their use case was to—they want their agents and all of their features to be able to do semantic search over a code base, right, or over multiple code bases. And that's what they use turbopuffer for. So if you've opened a code base in Cursor, then that code base is indexed into turbopuffer, right, like into vectors that are completely obfuscated and encrypted. And then they can use RAG over all of that to try to draw in more context. I use it all the time because, you know, you can ask in the chat, you can ask it something like, "Hey, what's the function that does this thing?" Like this morning I was asking, "What's the function we have that formats a number so it's not a million, like, you know, five zeros after or whatever?" But it's like it does some—and you just ask that in free text because you don't remember what it's called. That's the kind of thing turbopuffer is really good at, right? And you will see the Cursor agents make these kinds of queries. So that's one, right? So they're connecting code bases to AI, sometimes very, very large code bases or multiple code bases. Notion is another one of our customers, so they have a Q&A feature, and increasingly this is making it into more and more of their canvases where you can ask it, "Hey, what's the leave policy?" or "Hey, someone passed in my family, like what are my options?" It's used for an internal wiki; it can do research-type things. So they use it for that. And often the way that a person thinks about something and the way that it's written in a document are different, right? Like you search for "red dress" and they have a "burgundy skirt," like that's the kind of thing vectors are really good at. And so Notion uses that to draw context into the LLM to answer questions. Linear is another customer of turbopuffer. They use it for their search, and they also use it for similarity. So, "Oh, this might be a duplicate issue of this one," or "This might be the person who should work on this type of issue." I think they might also be using vector embeddings for that. So those would be some of the use cases that we've seen.

**Jordan Segall** [19:13]:

Here's, you know, we talked about why traditional databases aren't a good fit for sort of the SCRAP model that you talked about. What about incumbents like Elastic sort of going after and combining vector search with traditional search like you are?

**Simon Eskildsen** [19:25]:

If your per-user economics allows you to store everything in memory, then, you know, fine. That would make sense. Or on disks. I think traditional storage architectures, you know, they have lower write latency and they may have maybe more features, right, because they've been at it for longer. I think it makes sense for certain scale. But if you have the ambition to search potentially trillions of documents or billions of documents, then the cost might not make sense for your application, right? And you might have to start upping the price charges on your users in order to have this available, and you know your market better than us, right? And we just try to price against the first principle cost to us.

**Jordan Segall** [20:08]:

You know, I think you're one of the world's experts now in building object storage architectures. Any tips on just building that out for folks that are looking to?

**Simon Eskildsen** [20:16]:

One of the things that as an engineer continues to surprise me is how far simplicity goes. Now we have very high conviction that keeping everything on object stores, including the metadata, was the right decision. But really, our hand was forced a little bit early on because we had some customers that were growing very fast, and we didn't have time to introduce a separate metadata layer. And we thought that maybe people would care a lot about very low write latency. But again, we gained high conviction that this is actually fine. And the trade-offs and the simplicity that we get out of committing directly to S3 were there. So I think that let object storage surprise you. I think that there's a big bag of tricks that you started leveraging to build really scalable systems on top. The other day, for example, of course, we have many millions of prefixes on S3, and we sometimes have to go out and do various background activities on them, so then you have to go list them, right? The S3 API is like you start from A and then you list, and then you go and paginate through, and it would just take forever because for S3, this I think is sort of a brutal operation for them, so it takes a really long time. And so what we started doing was just have a read-through cache where every few pages we would just put in a text file what that prefix was, and then every time now we can sort of like start listing the buckets at all of these different keys in parallel. So there's lots of these tricks that you can use, but you like—I don't think I can explain to you how many hours I've sat and just looked at the S3 API to try and come up with something, right? Like another thing I'll say is like 404s are free, and you can use that to design systems, right? There's the compare-and-swap primitive. There's all of these little headers that you might not have paid attention to that allows you to build a system like turbopuffer with very, very low latency and very high durability. So I think we have a big bag of tricks. At some point, we should lay out the 16 tricks in the bag. There's nothing secret there other than just spending a lot of time looking at the S3 APIs and the GCS APIs.

**Jordan Segall** [22:27]:

And then we talked about sort of use cases that are a good fit for turbopuffer and not as much of a good fit. What do you think are sort of unsolved problems today within vector search?

**Simon Eskildsen** [22:35]:

So the hardest things about vector search is to keep the index up to date. So when you do vector search, the only way that you can guarantee that when you do this query that the 10 results you're getting back are exactly the 10 results that are closest to this vector is actually by looking at every single vector, right? It's an unsolved problem to have a non-O(n) algorithm that can do that. So we approximate, and we approximate with a number that the listeners may have heard of referred to as recall. Recall is basically, okay, these are the actual results I got back from my vector index, and here are the exact results. What's the percentage overlap? So if there's one result that was wrong compared to the exhaustive search, then you have 90% recall. Otherwise, you have 100% recall. We find that our customers feel really good at around 95% recall. That's sort of when you don't have to guess whether your evals are not performing because of your retrieval layer. And so in the background, even for a percentage of queries, we've run exhaustive searches against them and report them back to Datadog. So we always have an idea of what's actually going on in production. When you incrementally maintain this index, you have to make sure that the recall is good. And if you built the index, you know, knowing that, okay, I have an e-commerce store and they're selling pants and they're selling dresses and they're selling shorts, and suddenly they start selling shoes, well, which—like, how do we put this out in the vector index? Because the vector index sort of has these clusters around different things. And so maintaining high recall as you continuously update the data is very challenging, very, very challenging. In the beginning, in the first version of turbopuffer, once a certain percentage of the dataset had been overridden, we would rebuild the entire index. That's what most production implementations do today. But we found this very, very challenging and expensive to scale. So we spend a lot of time trying to implement an algorithm that will incrementally maintain all of the—like this ANN index, and this now works into the hundreds of millions of vectors and even into the billions, and we're trying to push as far as we can incrementally maintaining with high recall that ANN index because sharding is the only way to scale anything. But sharding too early is a cop-out, right? And on some of the traditional search engines, the shard sizes are maybe around 50 gigabytes, and you know, looking up an operation is log n or whatever it might be on. But if you have to do log n for a thousand shards, it's a lot more expensive than doing log n on five shards. So you want to make these shards as large as possible, and so incrementally maintaining high recall on a vector index at high ingestion performance in the hundreds of millions or billions for a single shard is a very challenging problem. The second challenging problem around vector search is filtering. So when you send a vector query, you are not just saying, "Hey, what's close to red dress?" Oh, it's the burgundy skirt. You're often like, "Does it ship to Canada? Is it red?" Like, you know, you have all these like real filters, right?

**Jordan Segall** [25:51]:

I love all these Shopify examples, but okay.

**Simon Eskildsen** [25:53]:

It's just, you know, I—

**Jordan Segall** [25:55]:

DNA at this point.

**Simon Eskildsen** [25:56]:

I grew up here right here in Ottawa is where the company was founded and the infrastructure was built, and it shapes your worldview. But anyway, so you do these like actual hard filters on top of this more fuzzy vector search, and that can be challenging because that's a different type of index that you need to use for that. And you can use all kinds of techniques to make sure that the recall is still high in the face of these filters because, of course, you know, if you search for something that's the color red, that might match a small percentage of the dataset, and you can just search all of that, so that's very easy. It's called a pre-filter. If something is—if you're checking for whether a product is public, well, probably 99% of them are public, so you just over-fetch a little bit and then you remove the others. But something like "ships to Canada," which maybe applies to 50%, that randomizes around the clusters, and you're trying to get a banana that ships to Canada, there might not be any produce that will ship because that will be prohibitive. So the clusters that are closest to the banana are just completely off. So what it ends up being is like maybe one of the clothing clusters, and it's like, you know, fruit-themed. I don't know, you know, just—and so providing high recall in the face of this really requires that the query planner that decides how to execute the query is aware of the filtering and is aware of the vectors. These are the two hardest problems. And I mean, like, capital H hard.

**E** [27:19]:

I mean, one thing I'm struck by, and obviously there's just so much nuance in solving this problem, is you think about this like zooming out this general problem of connecting data to LLMs. You know, there's a bunch of parts of that process that all have their own problems, right? There's like getting the data, you know, to the database in a way that works. There's obviously the embedding models themselves and how people do that. And I'm sure some people building in this space have said, "Okay, like, you know, customers like to have a one-stop shop, or we should, you know, build more of these things." It feels like you've been super focused on really just nailing this part of that entire process. Like talk a little bit about how you think about that and you think about like the future of turbopuffer across those vectors.

**Simon Eskildsen** [27:53]:

We've talked about simplicity a bunch. It's a very core cultural value, I think, both of the company and of Justine Li and I. Anyway, so simplicity and focus go hand in hand, and we felt that the hardest problem that our customer was having was not choosing the embedding model; it was not running the embedding model; it was not running the re-ranker; it was, "Hey, I need to store petabytes of data, and I need to search it." And so that's what we're focused on now. Over time, what we're starting to hear from our customers is, "Hey, for me to ingest 100 million vectors very quickly into turbopuffer, it would be really great if you could help us with that," right? So we wouldn't close—we're by no means not saying we will never do it, but we are very focused on just this because as soon as you have to start running the embedding model, then it's like, "Okay, for low latency, you might want to run it on GPU." Now you have to sort of get into that game and sort of the quant GPU game, right, that rules today's world. So we want to make sure that the things that we put our name behind, we're doing a really, really good job. I take the responsibility and Justine also takes the responsibility very seriously of people trusting us with their data. And we know that if we mess up, we're not the only ones that get woken up; our customers also get woken up. And every single decision that we've made around how we've designed this has been that we don't want to get woken up. I was on the last resort pager of Shopify for almost a decade, right? I have sat there at 3 a.m. debugging databases completely alone in the dark so many times. We know what that feels like, and we don't want our customers to be in that position. So reliability is number one. And if you put reliability as number one, then you have to put simplicity right up there as number two. And then when you start to bring in all of these other things, you better have gotten that right on the core product. So we take that very, very seriously, and it's part of—it informs everything around how we do it, right? The only stateful dependency that turbopuffer has is object storage, right? You can blow away all the nodes, and all of the data is safe, right? You can blow away everything, and things are fine, right? And routinely that happens in production, that things are blown away and autoscale down, whatever, and everything is fine, right? And no one notices.

**E** [30:26]:

Obviously, there's the ingestion and embedding. Is there other stuff that you think about like existing in this set of problems that eventually or customers are coming to you and asking about or a set of problems you eventually might want to add in?

**Simon Eskildsen** [30:37]:

Yeah, I mean, we'd like to just see what our customers actually use out there, right? It's like hard enough to find PMF on one product. So let's make sure that we double down on what's working. I think we see our customers asking us for advice on how should I do eval on search results? Our customers ask us, "Which embedding model should we use?" And we have internal reports, right, on which ones are fast. And what we hear, and so we can pattern match in a respectful way across what we see our customers doing. There are some embedding models from some of the labs that take 300 milliseconds to do a query; that's prohibitive for some search; that's too long. turbopuffer takes 10 milliseconds; it takes 300 milliseconds to create the vector; it's not acceptable. So we want people to use fast embedding models so that they don't get painted into a corner. Rerankers, the same thing, right? I mean, I worked on search at Shopify, and we see what others do in search here. And so we just help our customers. But in general, what I have seen and saw at Shopify as well is that in the traditional search engines, you end up with a massive DSL where you're expressing, "Hey, multiply the title, BM25 results with this and that and this field and then a little bit of the image," and it sort of becomes this like very finely massaged thing. And generally, it's written—someone sort of loaded that context into their head and then executed and got good evals, and then no one touches it for years because, you know, now you're addicted to what happens when you type in a set of characters and navigate. Have you ever tried when the search engine changes on you, and you just like understand how bought in you are to this? But it's very difficult to maintain; it's like thousands of lines of JSON or whatever. And so I think right now what we're seeing is that vectors make up for a lot of that. Again, coming back to this red dress, burgundy skirt example, well, we used a lot of effort in PhDs before to turn strings into things, but that's just, you know, you cut the head off of the LLM, right? And these numbers are exactly that. So we find that a lot of these features are not needed, and we find that our customers actually really like to just write a search.py or search.ts or whatever they're using and doing a bit more of this themselves. There might be, you know, one or two milliseconds of performance penalty, but fundamentally there's really not much, and you gain control. You can write tests. That's right. It's easy to write evals. So I think as we find that our customers want more of this and particular things where from first principles, it makes sense for that to be in the engine for performance reasons, then we will do that. Like something we're starting to see is that people want to do late interaction where you're often issuing something like 128 vector queries in one search query. And so that's maybe a little bit too much to funnel down over JSON. So we have to help with some APIs around that. So we're always paying very close attention to it, but we take the same stance as we always have. We don't, you know, European cities are beautiful because they're built incrementally, and software is really the same thing. When you start to guess too much about what people might do with the software, you end up with these, you know, like we're in Ottawa; it's like what, you know, it's not a particularly beautiful downtown where it's just like a lot of things needed to happen very fast, and so you build a lot very quickly. And I think a lot of bad software is written that way. You make too many assumptions about how people are using it, right? Look at like we didn't think that people were going to love the 100 to 200 millisecond write latency, but it turned out to be fine.

**E** [34:08]:

Does the approach to this kind of focus and gradual building, does that lead you more to then the cursors and notions of the world? One thing I've been struck by seeing infrastructural players kind of sell into large enterprises is I feel like they'll come with, "We do this part of your broader RAG solution," and the enterprises are like, "No, no, I want one thing to do end to end," you know, for me today. And I feel like a lot of, you know, in talking to a lot of insurance companies, they get dragged into other parts of the stack because that's what folks want to buy.

**Simon Eskildsen** [34:34]:

Yeah, I think naturally, right, everyone starts to bundle at some point, and you start to—you hear you see commonalities between your customers, and you try to help them get there faster. But you can also get too greedy, and you can start to do too much of this too soon at the cost of the focus of the team, right? What made it work in the first place was that you even have customers asking you for this, which is a blessing. So we think about it, and I expect us to like partner and bless and work on things to make the end-to-end process much easier for our customers. And yeah, it certainly takes a lot of discipline sometimes to say no and then say yes to working on more performance, more cost reduction, whatever it might be, right, on the core things that we want to get good at. There's a grab bag of ideas, and it's rare that a new idea enters the grab bag that we haven't already spent considerable time and effort thinking about. What is always difficult is to decide when to pull something out of the grab bag and to continuously do that Bayesian, you know, gradient descent on, okay, what matters this month, right? And continuously changing the priors based on what the market is demanding and customers, right? And not saying no forever. And we do exactly that process internally as well.

**F** [36:33]:

But I think this has been one of the hardest parts of building AI infrastructure companies generally is that, you know, with the underlying model layer changing so fast, the way people are using these models evolves so fast. And so it feels like you're playing a bit of whack-a-mole of like, "Okay, we just solved for the way people are building things today," and then in three months, they're actually building things in a very different way now. I think, you know, arguably the space you're building in might be the most isolated from that because it seems like at any capability of model, this will be required. But I'm curious as you zoom out and think about other parts of the AI infrastructure stack, like does that resonate, and how do you think about what other persistent parts of it will be versus, you know, things that seem more moment in time?

**Simon Eskildsen** [36:33]:

I like companies that have state. And that's why we built a stateful company. I think that there would be a lot of commonalities, right, of what would be in there. And I think that to build a good company as part of the AI stack, you either want to come up with a really good workflow, like something that makes—and lots of good companies have been built around workflow. Generally, workflow companies eventually start capturing some state as well. And generally, the stateful companies also start capturing workflow. I think that if you try to do all of that at once, you need a lot of years of R&D in a lab before you go to market. And I don't think anyone has time for that right now. So I think it's a very interesting time to be an infrastructure company because I think that among the frontier, they are picking and choosing and doing the Lego thing, and they're being very careful about what they're choosing, and they want the best in the world for every particular piece, right? And so then there will also be companies that try to bundle everything, but it will come at some quality trade-off for individual pieces. This market is different, right? In that, like, I feel like there's always some demarcation in the generation for different companies, right? It was very clear that a new era of companies happened in late 2022, right? It was clear that during this era, there was a particular set of companies, right? In the 2010s, which is where I feel like I grew up in the software world, you have the Stripes and the Shopifys and the GitHubs, Zendesk, and Pinterest, and so on that in some ways were also felt similar but also, you know, wonderfully different. And now we're in yet another one, and I think that it has various attributes, but I think that we are seeing that some of the ones that are doing very well are very specialized on what they're doing, right? They're taking the niche that they're good at and they're doubling down on it. At some point, we will see them bundle, but it might take a little longer than it did for the previous generation of companies because all the great generational companies have a very strong finger on the pulse of what's happening and what people want right now, and it's very informed by the customers.

**F** [38:47]:

One area that's getting a lot of interest now is memory on the AI infrastructure. What do you think the future of memory is? And then what is the role of turbopuffer, you think, in that space?

**Simon Eskildsen** [38:47]:

Yeah, I think memory—people will always start by playing around with the simplest thing, right? And so if you're using agents today that do memory, there's sort of—there's the memory within the context of a very long encounter, right? So if you're working with a coding agent, you might be working with it for a long time. And at some point, it sort of has to compact. And of course, right now for that kind of thing, a lot of you just use either a text file or you just ask the LLM to compact. And that's where it's going to start. Then we see them start to bring in memories sort of laterally, east-west to that chat itself. It's like, you know, my ChatGPT is very confused because I shared with my wife, and so there—but it's generated all these memories, right, of how do you grow this flower and what do you do about this pest, which is, you know, my wife. And then it will draw in, "Oh, since you're a good Rust programmer, here's like a script to get a weather and humidity report for your flower." Like, it's like, so now we have to split the account. But anyway, so those memories are lateral, and they're not—it's not a lot of data, right? And so even if you just pull them all into memory and did some similarity or pulled them all into context and they're condensed enough, that's probably fine. Are we going to start seeing memory at a scale where you have to start doing a lot of RAG over it? I think that we do see some of it. So one of our customers is this company called Portola, and they built Tolan, and Tolan has very long-standing conversations with their users. And these are not just memories, right? They're long, long chats. So there's also this sliding window between—or this slider between what's a memory versus just like searching in all prior context. I think similar to what you've heard me talk about before, we haven't seen enough patterns emerge here among our customers that we're going to—that there's anything in particular that we have to ship, right? You could also just put all of this into turbopuffer, and it's just like a key-value store on object storage, and it will work great. You don't need to use vector search or just—you could just use the keyword search as well. But I think it's TBD exactly what this looks like. I don't think—I think some implementations, it's not a lot of data, and some it is a lot of data, and it's more over the history. I think I would assume that those that do it over the entire history will outperform those that just condense into memories. But the memories sort of have a higher weight because they're condensed from a chat. I wouldn't pretend to know exactly where that's going to go, but we are seeing a lot of experiments in that area.

**G** [41:24]:

How much time do you spend thinking about where the foundation models are going in terms of, you know, obviously I feel like the future state of like the size of these models, the way they interact with memory, with, you know, databases—huge implications for your business, probably unknowable to some extent today.

**Simon Eskildsen** [41:40]:

I do spend a lot of time thinking about it. I don't think I have a general answer to just like, "This is why it's fine." I think it's easiest to think sometimes in extremes. Okay, we get AGI. Well, none of this matters anyway. So, okay, right? So that's fine. Great. You know, like everything you—we have now will just compound, and everything's great. Then there's the other scenario, right, which is I think the one that we're rapidly entering now where the models are like incredibly powerful. They're very good at generating reports over large amounts of data, and it's just very clear that even if they get very capable, it just feels like you've got to yank a pipe into them and put something computationally good on the other side. And like I think the architecture could look a lot like turbopuffer because there's a lot of data you don't need all of it at all times, and it needs some kind of fuzzy search a lot of the times. I think that there's a real role for a database with this architecture. I think that to build a good database company, you need two things: you need a new workload, which is connecting lots of large amounts of data to LLMs, and you need a new storage architecture. And we talked all about that before, right? And why now for it. And so I think our hypothesis is that this will play a role in how these LLMs are going to interact with data, right? But we also see lots of people who just use turbopuffer for traditional search, right?

**G** [42:52]:

Before we started recording, you kind of said there would be this new set of things that would be table stakes for SaaS applications. And, you know, I imagine one thing that you think about a lot is just this flowering of tons of different applications that will need hundreds of millions of vectors and, you know, build applications on top of that. Like, how do you think about the, you know, addressable universe of what those companies might look like?

**Simon Eskildsen** [43:13]:

When I think about what I want out of some of the applications I want, like today I'm just like, "Oh, like I just talked about talking with someone about this over here." Like, can we just—you're essentially—a lot of what knowledge workers do has to do with funneling context around in different systems, right? So it'd be great if they could help with that, and I think we could help them help people with that. I think there's a bunch of features that are now going to be table stakes for SaaS in the same way that once mobile really hit, it became table stakes that everyone had a good mobile app. And it felt like a huge tax at the time, right? I have to bring in another programming language; I have to do this. And there's all of these things that were happening around the time around like, "Do we build them natively? Do we build them as web?" And now, you know, you just don't think about it when you build a software company that you're serious about; you just kind of need a mobile app, right? For most of them, not all, but for most. And I think that that's also what we're seeing with AI, right? It's that there is now a set of table stakes features that people expect there to be in your application in the same way that if they expect to search for your application name in the app store. Those features are things like I think that semantic search works, right? If I search in my Linear for chat, issues tied to Slack also come up, right? You know, if I search in a commerce store for burgundy, you know, whatever, right?

**F** [44:24]:

Yeah.

**Simon Eskildsen** [44:24]:

If I search for a shoe and they only have sneakers.

**G** [44:37]:

This must have been the most important eval at Shopify.

**Simon Eskildsen** [44:43]:

I think it's just one that I keep coming back to. I think semantic search is table stakes, right? And I think it works great as a byproduct of the LLMs. The second one is similarity. I think if people are just expecting this deduping and, "Oh yeah, there's something similar to this over here," you could also call it recommendations; it's, you know, a rose by any other name. The third thing you want is that you want the ability to generate a report, like sort of ask a question, and then, you know, it goes and finds a bunch of information, queries the data. Then you also want some agentic workflows, right? Cleaning up things like taking actions for you and all of that. And the agentic workflows probably want some of one and two and maybe also three to get them done. And there's probably others that are idiosyncratic to the particular application. But I think all of those are becoming table stakes AI features in SaaS. And I think we're seeing that the incumbent SaaS providers are doing a phenomenal job at building these in and really prioritizing it, and I think that the upstarts—there's a massive opportunity to try to get some of these like really, really right and build interfaces that are native around them. But that's how I think about the AI era SaaS, yeah.

**G** [45:54]:

Yeah. What do you see with multimodal data? Is most of the usage of turbopuffer today text-based, and like, you know, where do you see that going?

**Simon Eskildsen** [45:59]:

Yeah, I mean, it's completely possible to do something multimodal in turbopuffer. Again, I look at what the market is doing, not what they're saying. And I think that we don't see that many companies yet who are doing multimodal, like over images, over attachments, and all of that. Usually, the implementations lag a little bit behind, right? But I think it's great, and I think the economics of object storage make it really, really nice that you embed, you know, both the picture of the product and the description of the product and all kinds of other attributes around what you're searching. You know, the economics of turbopuffer might allow you to just embed all the PDFs and not think too much about what it's going to cost you, but that's otherwise been scary because, okay, someone just uploaded a 2,000-page PowerPoint presentation; are we just going to embed that and like not charge them extra? Like you don't expect all your SaaS providers to start doing usage-based pricing, right?

**G** [46:56]:

Yeah. Well, we always like to enter interviews with a standard set of quickfire questions where we basically just cram in all the questions that we didn't have time to hit in the regular interview. What company do you think would be most interesting to run AI at?

**Simon Eskildsen** [47:06]:

I mean, it would be one of the frontier labs, right? It would be like OpenAI or Anthropic or one of the ones that are just seeing the models of three or six months out.

**G** [47:15]:

Where's the name turbopuffer come from?

**Simon Eskildsen** [47:17]:

Do you want the real reason or do you want the marketing reason?

**G** [47:22]:

Definitely the real reason.

**Simon Eskildsen** [47:23]:

The real reason was that it made me happy, sounded funny, and it had an emoji that had no other real meaning.

**G** [47:31]:

That is a good emoji.

**Simon Eskildsen** [47:32]:

And then how have you made—have you turned that into great marketing? When the pufferfish is deflated, it's on object storage, and as it expands all the way into battle stance, it's in DRAM, right? And SSD in between.

**G** [47:43]:

Nice.

**Simon Eskildsen** [47:44]:

You must love—

**G** [47:44]:

You must have been very proud of yourself when you came up with that.

**Simon Eskildsen** [47:46]:

Yeah, you know, it's just—yeah, maybe a little bit, but it was not the original intent of the name.

**G** [47:53]:

What's one thing you've changed your mind on in AI in the last year?

**Simon Eskildsen** [47:57]:

You know, I'm getting a lot of—like on AI, I spend most of my time still thinking about databases.

**G** [48:02]:

Yeah.

**Simon Eskildsen** [48:04]:

And I think the biggest thing that I've changed my mind on in databases is I just keep being surprised that this simple thing continues to work. And it's not a great answer because it's not a good gotcha, but that would be my answer.

**G** [48:17]:

Yeah. What do you think is the biggest mistake you've made so far in running turbopuffer? Or something you look back on from a few years ago and you're like, "I wish we learned that lesson."

**Simon Eskildsen** [48:25]:

I feel like we, on the product at least, haven't committed any major mistakes yet. And I think people sometimes underestimate how hard it is to run product early at a startup. But the first few customers that we had used every single feature of the product, and there is not a line of code that wasn't being run in production. If I thought about it a bit harder, because we definitely made a million mistakes, I could come up with a better answer for you. But I think a lot of it is the survivorship bias of getting the product right.

**G** [48:58]:

What was something you learned as a founder?

**Simon Eskildsen** [49:00]:

I get a lot of people who tell me what they think that I should do. And—

**G** [49:04]:

VCs.

**Simon Eskildsen** [49:04]:

Yeah, especially VCs, you know, and I've really learned to trust my instincts. And I think that when we talk to the team and we have a feeling about something, and just giving everyone the permission is like, "Okay, let's just try it." That's worked great. Everyone says you should do embedding; you should do re-ranking and all of that. It doesn't quite feel right yet. The vibes have to be right.

**G** [49:36]:

Yeah, I'll let the vibe—always about the vibes. I assume you think about a lot of like, you know, questions about the future of where AI is going. If you could, like, you know, talk to someone from the future and get one question answered that would, you know, help you in building for whatever today, what would the question be?

**Simon Eskildsen** [49:51]:

How much is the agent searching?

**G** [49:55]:

The extent of vectors that the agent has to go through.

**Simon Eskildsen** [49:58]:

Not just vectors, but like how much is it utilizing a search engine, right? Like it's very clear that you're not going to do web search by loading that entire thing into context, right? How much are they searching?

**G** [50:09]:

You have an interesting story around how you learned how to code. You know, first you started on the online PHP resources, then you took a break because there weren't any more resources, you played a lot of World of Warcraft, which I was a big fan of, by the way, and then you learned English from that, and then you got back into coding. How do you sort of think LLMs will change how people learn to code, and then what do you think sort of the future of software will—software engineering will be with LLMs?

**Simon Eskildsen** [50:31]:

There's nothing more that I would have loved than an LLM to talk to when I was 11 trying to learn how to program, and just like the Danish web on doing web programming was too small. I just like—I mourn for my younger self to not have had an LLM to learn with. Like, I think about that a lot, and I think about it in the context of like my daughter and just like how a curious child now can just get access to so much in such an accessible form, and that brings me a lot of joy.

**G** [51:02]:

That's awesome. Well, this has been a fascinating conversation. I'm sure folks will want to pull on all sorts of different threads. I want to leave the final word to you. Where can folks go to learn more about you, turbopuffer? The floor is yours.

**Simon Eskildsen** [51:12]:

Yeah, so turbopuffer.com to learn more about the database, the trade-offs of it, and what it costs and everything along those lines. I mostly post on X, so x.com/sirupsen. turbopuffer is also on there, but turbopuffer.com is the best way, and on X, yeah.

**G** [51:29]:

Amazing. Well, thanks so much. This is a ton of fun.

**Simon Eskildsen** [51:31]:

Thank you so much for having me.
