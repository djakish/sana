# Object storage-native database for search

March 09, 2026•CMU Seminar with Andy Pavlo

[Video 3](https://www.youtube.com/watch?v=pqoRNwNaxfs)

## Transcript

**Andy** [0:26]:

Let's get started. We're excited today to have the CEO and co-founder of turbopuffer, Simon Eskildsen. Simon started turbopuffer three years ago. Prior to that, he was eight years at Shopify, where he was principal software engineer and director of engineering. So a lot of things I'll talk about today are the things he learned about at Shopify. As always, if you have any questions for Simon as he's given his talk, feel free to interrupt him at any time. That way, he's not talking himself for an hour on Zoom. Simon, the floor is yours. Thank you so much for being here. I appreciate it.

**Simon** [1:00]:

Thank you, Andy. Thank you for having me. I just want to reiterate that if anyone has questions or anything during the talk, I very much encourage that. I don't want to talk just for an hour with no interruptions. But otherwise, I know Andy won't be shy to take any questions from the chat. So please, my name is Simon, and I will be talking about turbopuffer today, a database that we built on object storage that's currently primarily focused on search.

**Simon** [1:28]:

So turbopuffer is a database that's mainly optimized for search, while it can do a bunch of other things as well. I think the easiest way to show what it can do is just a simple full-text search query like this, where if you are searching for something, you can imagine that you might need to rank by various different signals. You have some hard filters on, you want some timestamps after a certain period, and you want to limit the results to something. You can do this with vectors; you can do arbitrarily different kinds of full-text search queries, like ranking by boosting results that have a more recent timestamp. All of these things are very useful for searching. But you can also do more than just search in turbopuffer. You can do aggregation, you can group by different attributes, you can do ordering by any type of column, and even multiple columns. So turbopuffer has lots of other features when your data is in it.

**Simon** [2:20]:

I think that in the limit, a database ends up more or less implementing every query plan on the data structures that are implemented inside of the database, but you have to start with a very particular workload, and that workload for turbopuffer right now is search. The other thing about turbopuffer is obviously upserting your data into the database. You insert your data through turbopuffer; you don't just put it on object storage and then turbopuffer fetches it. You have to do it through turbopuffer itself. We provide a variety of different queries while writing that you can do.

**Simon** [3:00]:

For example, something that's very common when you're writing into a search engine is that you only want to write a record if the version that you're writing is newer than the version that's in the database, right? So you can imagine that you have a system where Shopify, for example, had a million of these systems where every time someone changes a piece of data, they write it into the database, and they have bulk ingestions into the background if you want to make sure that only the most recent document survives. You can do a variety of other things like doing patching of attributes with a filter, right? This is like an update where, and right now the JSON API to turbopuffer is fairly simple, and over time we expect that we'll probably ship a SQL interface into turbopuffer as the queries become more and more expressive.

**Simon** [3:50]:

I wanted to also just frame a couple of the numbers around the scale that turbopuffer runs at just to try to contextualize what kind of environment we're making decisions in and what we're scaling for. As far as I'm aware, we host the most vectors in the world with almost 3 trillion, or actually more than 3 trillion vectors and documents with full-text search in production. We run these at tens of thousands of queries per second, and we see peaks just shy of 100 GB per second. We also have more than 200 million namespaces. These namespaces you can think of as shards or tables, and we'll get into more depth of them later.

**Simon** [4:30]:

The largest search spaces that we see are essentially people ingesting all of Common Crawl into turbopuffer and searching it at once. This is often in the high tens of billions of documents that you're searching at once, which poses lots of challenges that we're going to talk about today. This talk is composed of three different parts. The first part, we'll talk about V1. V1 was the version of turbopuffer that I wrote at my cabin up here in Canada over the summer of 2023 and put into the market to see if anyone would care about this kind of database. We took that to its limit and eventually turned it into a real database with V2.

**Simon** [5:10]:

The other thing I want to connect this to is Andy's theme of basically justifying turbopuffer's existence against being a Postgres extension or merely just Postgres. Why build another database, and what kinds of conditions have to arise for a new database to be successful, both commercially and also usefully to companies around the world who are building software around it? So first, we will talk about V1. To properly, I think, understand the database, you have to understand the DNA and the origin of the people who wrote the database and what kinds of scars or experience, whatever you want to call it, they go in with as they're designing the system.

**Simon** [5:50]:

My background is not in database research. It's not even in working on databases. I didn't even go to university. My experience is fully from operating these databases and building layers on it for scale. I worked at Shopify between 2013 and 2021 while we were scaling from a thousand requests per second to more than a million. The hardest part of these apps to scale ends up being the databases. That's what's most likely or causes the most painful outages on Black Friday, and that's what we're always preparing for. Through my time there, I worked primarily with the databases below: MySQL, Redis, Elastic, Memcached, Kafka, Zookeeper, and many others, and learned to combine them in all kinds of creative ways.

**Simon** [6:30]:

I think that being on the last resort pager, where I've been paged by every single one of these databases many, many times, just irrevocably has changed the way that I write software and also the way Justine Li, my co-founder, writes software. I have debugged every single one of these databases for hours on end, completely sleep deprived. I have been woken up by some of these databases every single day in the days leading up to Black Friday. It's taught me pretty much everything that I know about how to design systems that do not wake you up because that's what you end up optimizing for. So for better or worse, this is one of the most important things that I design software now with in mind.

**Simon** [7:10]:

The other thing that I learned is that it's very uncommon to have this long of a tenure at a tech company these days. Eight years working on a single piece of software and scaling it teaches you a lot about how software ages well or what doesn't age well. I think my biggest takeaway from this, which is quite congruent with being on the last-resort pager, is that it continues to shock me how the things that you put in place three days before Black Friday, with spit and bubble gum, as my boss used to call it, tend to be some of the things that last the best.

**Simon** [7:50]:

I keep saying about my co-founder Justine that she is probably the best person I've ever seen come up with one-line fixes to 100,000-line solutions. This continued to surprise me again and again how something very, very simple, when you think hard enough about the problem, can age very well compared to the RFC that's been debated among ten principal engineers and gone through revisions by every team before they make it into production. Every single one of these databases has heavily influenced the way that we think about designing turbopuffer.

**Andy** [8:20]:

I mean, I don't want to like, you know, sit and rag on other systems, and that obviously depends on many different factors of where you deploy them, but which of these six was the worst?

**Simon** [8:32]:

The one that I now compete with.

**Andy** [8:36]:

Okay. Then we're going to go scars with. Okay. Sure. Yes. Fair.

**Simon** [8:40]:

The other major thing that goes into designing turbopuffer is this project I run called Napkin Math. Napkin Math is essentially a collection of different numbers of how a machine is supposed to perform. There's more than the ones that are right; they're just some of the most applicable to turbopuffer. Napkin Math is basically just a collection on, okay, what kind of bandwidth can I expect on an SSD read, right? Not what the device says on the drive, but what a reasonable amount of code would do. What can I expect from object storage? This number I didn't get to update in time, but you can expect about 100 MB per second from a single connection, but of course, you can exhaust the NIC if you use as many connections as possible.

**Simon** [9:30]:

The p99 is around 50 milliseconds, and that doesn't vary very much between a block size of 128k and 1MB. All of these numbers are maintained in this repository with the first Rust that I ever wrote. The reason that I worked on this was because in my role as a principal engineer at Shopify, what I spent a lot of time on was reviewing how people were going to use some of the databases we just talked about before for developing some feature. Often they would show up with a profile of, okay, I did the workload in this way on this database, and then they would make decisions based on these benchmarks.

**Simon** [10:00]:

Often those benchmarks were wrong, and what I found myself doing again and again was just saying, well, look, it has to visit the B-tree this many times; there's no way it's going to take more than this amount of time. Then sure enough, you look at the query plan, and the database wasn't doing what it was supposed to do. So I collected all these numbers as a way to basically construct a model, a very simple model of the system, and then construct a very simple hypothesis for how it should perform. Then you see the difference between how the system is actually performing on a query and what the math is telling you, and if that gap is off by an order of magnitude, then you know that either there's a massive opportunity to improve the system or there's a bug in your understanding and your model of the system.

**Simon** [10:50]:

This always beats profiling. Profiling will tell you how to optimize the system 10%, 5% here or there, but it will not tell you what the absolute floor of latency or performance is. The other thing that was in the air was building a database with a different kind of design. Back to my scars of being on call for a very long period of time, it seemed clear to me that the only database that I would go on call for was one where I didn't have to operate the storage layer, where the storage layer was entirely reliant on S3, including even the consensus layer.

**Simon** [11:20]:

When I think about an object storage native database, the way that we've decided in turbopuffer for maximum operational simplicity and for reliability, an object storage native database is one where object storage is the only stateful dependency. There's no east-west coordination. There is no consensus layer in a separate system. Everything about the system is in simple files on object storage. The other condition for an object storage native design that I sketched out in 2023 was that the storage engine and the coordinator planner should be object storage aware. It should know that the round trip p99 to object storage is around 100 milliseconds. It should know that those round trips are costly, but concurrency to the system is heavily encouraged, right? If you can minimize the number of round trips but do a lot of outstanding concurrency, that's really good for object storage.

**Simon** [12:10]:

So the query planner and the storage engine should be deeply object storage aware in a way that's not possible to easily bolt onto another system. The third condition for an object storage native database is one where coordination is done with compare and swap directly on object storage. Modern object storage today has these operations where you can essentially fetch an object, you get a tag back, and then you only write the object back to object storage if the tag is matching. This allows you to drive consensus with object storage at the cost, obviously, of latency. But if you carefully design the system, you can still get very good performance with this primitive alone.

**Simon** [12:50]:

This kind of database is only one that you've been able to build as essentially December of 2024 when S3 finally released Compare and Swap. Google Cloud Storage had this available beforehand, which was actually not because of, it was just because that was the one I was familiar with at the time when I started turbopuffer because Shopify ran on GCP. NVMe SSDs were also not available in the clouds until 2017, and most databases today still aren't running on them. They're all running on these network disks that are not particularly fast. S3 also didn't become consistent until 2020, meaning that if you wrote an object, you're not guaranteed to get the same thing back in the next read, which meant that all of the databases built on object storage before December 2020 had a fat metadata plane to coordinate all of this.

**Simon** [13:40]:

These conditions allow us to create a database that is completely different from the ones that go before it. It has tradeoffs, and I think every database, when I look at the website, sometimes I feel like when I'm looking at a database website, it's difficult to figure out if they're selling a sneaker or a database. To me, what should be very clear is that every database makes very, very different tradeoffs, right, in terms of how they store the data and the storage architecture that they've made. The trade-offs for turbopuffer's storage architecture is that it's very low cost. Everything goes to object storage, $0.02 per gigabyte to store. It's very simple. You could blow away every single VM in turbopuffer's accounts, and we would not lose data because we will never acknowledge a write to the writer until it's been acknowledged by S3.

**Simon** [14:30]:

Simple horizontal scalability because all of this is sent to the hundreds, if not thousand-plus engineers that work on S3 or Google Cloud Storage. Warm queries, there's no reason when they're in cache in DRAM and in the NVMe SSDs that it's as fast as any other system. Write throughput can be phenomenally large, right? turbopuffer just closed to 100 GB per second of peaks. The weaknesses are very clear, right? You have slow cold queries when you go directly to object storage. The p99 of reading a block from object storage is around 100 milliseconds, and it can spike higher than that if you have a lot of objects on a node, and there's a lot of opaque caching at play. So a cold query to turbopuffer can easily be 500 to 1,000 milliseconds.

**Simon** [15:10]:

You have high write latency. The p99 to a put on object storage is around 100 milliseconds for a small write, and obviously this can get larger as they're larger. And so this prohibits certain types of workloads, right? You could never just hot swap a relational database, even if turbopuffer supported all of SQL. Something like Shopify would just never work on a database like that because the write latency would be too high for doing things like inventory reservations. And then the last thing is that because we have to drive consensus with object storage, it means that to get consistent queries, we have to round trip with a conditional GET (If-None-Match) to make sure that the caching node has the latest write, which takes about 8 milliseconds on S3 and about 15 milliseconds on Google Cloud Storage.

**Simon** [15:50]:

You can mitigate these weaknesses in the limit, right? You can pin things in cache, and you could implement another type of east-west coordination or something like that to try to drive consensus. But in the object store native design that turbopuffer is using, these are the fundamental weaknesses. But these are very good trade-offs for search. So to go back to starting to write V1...

**Simon** [16:40]:

So...

**Simon** [16:40]:

please.

**Simon** [16:41]:

we have a question from the audience. Do you want to unmute yourself? Go for it.

**Audience Member** [16:47]:

Well, I just had a quick question. So there were strengths and weaknesses. I saw a high write throughput in the strength, but there was a high write latency in the weakness. So I was just curious, like these two seem to collide, and I was just wondering why.

**Simon** [17:04]:

Yeah, so we can basically, you can max out the network, right? So you can write, if you, modern VMs, the ones that turbopuffer runs on, have maybe 50 gigabits per second, right, of networking. And so we can max out that and write that into object storage. But in order to actually, even if you did a small write of just a few kilobytes, and we have to round trip to S3, we have to wait 100 milliseconds. So it's sort of a classic throughput-latency tradeoff where if you're writing 1 MB or 100 MB, we might be able to do that in the same amount of time, even though in another database those would have very, very different characteristics.

**Audience Member** [17:44]:

Thank you.

**Simon** [17:49]:

So the problem that turbopuffer initially was trying to solve and is still solving is one of reducing the cost of search dramatically. I knew this problem from the back of my head from my time operating the Lucene-based clusters at Shopify and the operational woes of doing so. But when I left Shopify in 2021, I was helping a bunch of my friends' companies with their database struggles, which in 2021, by the way, mostly boiled down to tuning autovacuum in Postgres. One of these problems was my friends at this company called Readwise. We index a bunch of articles and wanted to build search, and in particular vector search over all of it. I ran the back of the envelope math on it, and it would have cost 30 grand a month to index all the data. This is a bootstrapped company that was spending five grand a month on all other infrastructure combined.

**Simon** [18:40]:

So just doing search for 30k a month was just not tractable. It was completely bonkers to me that it would even cost that much to put all these vectors in. And it occurred to me that it was because the incumbent solutions were all doing all of this in memory, even though it seemed particularly well-suited to me to a completely different architecture. These were kind of the things that I put together to start the first version of turbopuffer. If we look at sort of the math on the cheapest way to store and the most expensive and most performant way to store, the classic way to run a database is that you replicate it among three SSDs, and then you have up to 100% of the data that's hot in RAM. If you run that on a modern cloud, you might be running north of between $2 to $3 per gigabyte that you're operating.

**Simon** [19:30]:

This is two orders of magnitude more than having all of that in S3. Now, of course, S3 has completely different performance characteristics, right? High throughput, high latency. But it felt to me that you would be able to create a database that would be able to puff up in between these layers of the memory hierarchy based on, okay, Andy's nodding; he's getting the name now, maybe even turbopuff up between into the NVMe SSD and into memory. I think one of the fundamental trade-offs that a lot of the database systems make, right, is if you think about the hierarchy between object storage, the SSD, and memory, you want to try to keep as little as possible in memory and disk, right, and as much as possible in object storage.

**Simon** [20:20]:

And so the cache efficiency of the system is very important. But over time, you know, in the limit, if you're doing 10,000 QPS to a workload, the economics of DRAM makes sense. If you're doing a query every hour, probably it makes sense to have it in object storage. So if you can build a system that turbopuffs into the memory hierarchies at the right time, this could be very, very useful for search. Think about one of our customers like Cursor or Notion. When you open a Cursor workspace, right, it doesn't need to be in memory until you're starting to query that code base a lot, right, and all the embeddings for it. Same with Notion, right? When you search turbopuffer, right, we can know ahead of time to start warming the caches as you type into the query.

**Simon** [21:10]:

So there's lots of tricks here that we can play to warm it into the memory hierarchy, and if something is busy, well then we can have it in the memory hierarchy at all times. The first version of turbopuffer was very, very simple. It ran on an 8-core commodity instance in GCP, and this is simple, but it's not irresponsible, right? Every single write was committed to S3, and this object storage native model allows you to create really simple but still very reliable databases. turbopuffer at the time was really novel in charging about a dollar per month per million vectors. Some of the other incumbents were charging close to a hundred dollars for storing something like that, and so that was what really brought this onto the map.

**Simon** [22:00]:

I should also mention that really this was more of a summer project. I did not intend for this to become a big company. It was really just a, I want to see if I can do this because it seems fundamentally possible, and the idea just completely consumed me. As you can see here too, I don't say that it's the fastest. A lot of databases are like, "Oh, we're so fast." It's like, "No, it's reasonably fast." But the cost unlocked new use cases that I'd known right from working with Readwise. So now we'll start to get into the meat of the technical implementation of V1. Some ideas survived, and some did not.

**Simon** [22:40]:

One of the things that survived is the fundamental architecture, right? I mean, pretty soon after it was launched and there was interest, we ported this over to something that would run on multiple nodes, but this architecture is largely the same today. We've evolved it from here, but this is the current form, and it served incredibly well. If we follow a read through this, you can imagine it first hitting the load balancer. The load balancer will take the table or the namespace, as we call it, that you're querying and then do a consistent hash on it, right? Proverbially a consistent hash. You can imagine that this is arbitrarily complicated to try to get you to the node that is most likely to have that namespace in cache.

**Simon** [23:20]:

So it will go there and then it will start reading the metadata and then fetching all the different blocks that are required to service that query, going through the memory cache, then the NVMe cache, and finally going to object storage for the files that it needs, or the subsets of the files that it needs to service that query. So if you do a cold query on a million vectors or a few million vectors, it might take about 500 milliseconds to do the three to four round trips to object storage. If it's in cache, it will take about 10 milliseconds. The majority of that time is spent waiting for S3 to round trip to make sure that we have the latest data. If it's in disk, it might take a little bit longer, right, like maybe 20 milliseconds or something like that. But just to give you an idea of the profiles through the cache here.

**Simon** [24:10]:

For a write, the path is more or less the same. When it goes to the same node that would be doing the queries, we write it directly to the write-ahead log on S3. So you could imagine sort of like 1.bin, 2.bin, 3.bin; we just continue to append into that directory. When it's been committed to S3, we'll send back a success to the client, and then we will write it into the cache on that node if we think it's prudent to do so. If there's a lot of reads and writes going on at the same time, it's a good idea to go straight into the WAL with any new writes.

**Simon** [24:50]:

When the WAL is far enough ahead of where we've last built an index, like the vector index, full-text search index, attribute indexes, and other secondary indexes that we might build, it will send a, it will put it into the queue. The queue also exists on object storage, and the indexing nodes, sometimes in other designs called the compactor nodes, will work off of that queue and do things like compaction, building indexes, and things like that. And similar to the query nodes, there needs to be some affinity. The thing that I love the most about this design is that we can just move namespaces around by changing where they are in the load balancer.

**Simon** [25:30]:

At Shopify, we wanted to change where a shop lived, right? We had to move all of the data between MySQL shards. But with this design, we can just do a cache miss or start warming another node. The other thing that's really nice about this design is that if you want replicas, you're really just round-robin to multiple nodes, and they just start picking it up from cache. So if we have too many reads, we can just start spreading out the load, and obviously, it also makes deploys really, really nice because we can just roll through the whole system at some pace and just manage the cache fit rate.

**Simon** [26:10]:

The write-ahead log has also survived from V1. It's really about as simple as you might imagine. We don't actually use 0 and 2 and 3.bin. These are UUIDs committed into a manifest, but for simplification here, this is how the WAL progresses over time. Most databases do this, at least MySQL does this, where you're accumulating writes into a buffer, and then you sync it periodically. So in something like MySQL or on a disk, right, if you have a bunch of like 100-byte writes, you're not fsyncing every single time you get a hundred bytes; you will try to accumulate some to fsync a larger buffer.

**Simon** [27:00]:

It's really the exact same design on object storage, just rolled up into latencies in the hundreds of milliseconds instead of in hundreds of microseconds. So that's what we do, right? When you're doing a lot of individual writes, we'll coalesce them into a buffer, and then we'll commit them to object storage. All the different object storages have very different characteristics on how you can commit to the WAL, but it will just continue to progress like this where we commit a file and then commit it into a manifest with compare and swap. When you do a read, we will do a consistent read on the manifest that has an inventory of the WAL.

**Simon** [27:40]:

That we can do with a conditional GET (If-None-Match), which only goes to the metadata layer, which has better performance than actually loading the file, which has to go through the storage layer of S3. These are different latency profiles on all of the different object storage implementations. S3 is about 8 milliseconds, GCS is about 15 to 18 milliseconds, Azure is actually 5 milliseconds, last I heard. So we get that, and then we play the amount of the WAL over what we got back from the index to make sure that the query is consistent.

**Simon** [28:10]:

This is a bit of an unusual design choice for most search engines that just have eventual consistency. But we've worked around that before, like when working with Elastic and other solutions like that, working around eventual consistency is such a pain that we did not have high conviction in giving that up. You can give that up, and then your queries will be faster. But by default, turbopuffer is strongly consistent and will replay the WAL entirely on top of the index read.

**Simon** [28:50]:

Another design characteristic of V1 was the implementation of or the idea of essentially giving access to unlimited shards. When Justine and I were designing different systems at Shopify, we were always just abusing the shop ID key as much as we possibly could. I've always wanted a database where that was available as a low-level primitive where every single piece of data that is logically divided from everything else could be completely separated. Similarly, also the primary key matters. This is one of the things I love the most about MySQL over Postgres is that it's very easy to organize all the data by the tenant ID, which in Postgres you'd have to rewrite the entire table to do, or the entire heap.

**Simon** [29:30]:

So turbopuffer exposes this as a first-level primitive. When you work with a namespace or what you might call a table or a shard, it has a particular prefix on object storage. Now, this is not actually what it looks like, right? You have to randomize some prefixes and things like that to get it to be performant. But this is the simplest sort of lie to children metaphor for what we do. We have a bunch of metadata files, the WAL is growing, the LSM has a bunch of SSTables, and then you have a large metadata file that sort of spells out what keys are where, right, in a very, very simplified LSM 101 type of design.

**Simon** [30:10]:

We have hundreds of millions of these namespaces in production, and maintaining all this metadata is certainly a challenge, but it allows for really good cache efficiency. Between use cases like Notion and Cursor where all the data is so logically separated apart from each other, and your bin packing problem becomes a lot nicer and more tenable.

**Audience Member** [30:03]:

What is the metadata challenge? Is it because S3 just chokes on it because you have so much crap in it? Or is it your side maintaining the in-memory representation of what's out in the namespaces?

**Simon** [30:14]:

So you're going to have to once in a while loop through all of these things for like once a day or whatever to do billing or different reconciliation. The list call to get a thousand entries takes somewhere between 500 milliseconds and a second. So just paginating through the whole thing requires you. I have a bonus slide if we have time of what I call the object storage bag of tricks. But you have to play a bunch of tricks still doing this on object storage entirely to manage that. For example, these are the kinds of challenges that we would have, right? And then in the billing layer, it's very high cardinality as well, yes.

**Audience Member** [30:51]:

So if Amazon magically made the metadata lookup be sub-millisecond, your life is easier.

**Simon** [31:00]:

I would say so. It would be a lot easier. There would be a lot of things that we would not have to design around. The list call is not one that I recommend ever using in production on a critical data path.

**Audience Member** [31:10]:

Okay, awesome. Thanks.

**Simon** [31:14]:

The other thing that's very much survived from V1 and continues to be important in turbopuffer is to have very fast cold queries and to use object storage native query plans. The general insight here is that you want to minimize the number of round trips to object storage as much as possible, right? As was asked earlier, you can drive a very large amount of throughput. You can saturate the NIC, but you're not going to be able to do it in less than a few hundred milliseconds of latency. So you better make good use of it.

**Simon** [32:00]:

Now fortunately, this is also how NVMe systems work, that if you do a very large amount of outstanding concurrency and few round trips, they will perform incredibly well. But if you have a lot of dependencies in your I/O chain, they're not going to perform as well. So it just seems to be the trend all, I mean also in CPUs, right? Where if you can do a lot of dumb simple operations and very, with very, very little dependencies between them, the machines rip, and that's been our experience with object storage, NVMe, and also the CPU.

**Simon** [32:40]:

So everything is designed around this, and we're designing it even that these cold queries are quite common. So you can imagine in the first round trip, you get some metadata around like, you know, that lsm.json and the index.json. These might not actually be JSON files, right? But just for illustrative purposes, and then you get the upper layers of some tree, lower layers of some tree, and then maybe you, you know, you have select star or whatever, you get a bunch of data at the end if you couldn't speculate earlier on.

**Simon** [33:10]:

The second thing that you need for fast queries to object storage is that you're only going to get about 100 MB per second from every connection that you open. So if you have a large SSTable and you want to download it fast, you have to split it into a bunch of different ranges and then parallelize all of those requests. Subsequently, if you are just fetching a little bit of data, you might as well fetch the entire block and even adjacent blocks because fetching a megabyte of data might have a p99 of, say, I think it's around 80 milliseconds, right? Whereas fetching much less has a p99 of maybe 60 milliseconds.

**Simon** [33:50]:

So it can be advantageous to try to fetch more data at a time. Large block sizes, right, the p99 just does not vary very much between getting 16 kB and a megabyte. And then it's also important for all of this to be zero copy. A lot of tiered databases will sort of download something from S3 and then take its jolly time decompressing it and hydrating it into memory and then serving queries. Not all of them can just do a direct range read and then just start operating on that data immediately because they haven't been designed for it from day one.

**Audience Member** [34:30]:

Are you using io_uring to avoid zero copy from the NIC?

**Simon** [34:34]:

We haven't even needed to use io_uring yet. We're just using direct I/O. It has not become a bottleneck yet for like, this has surprised me.

**Audience Member** [34:41]:

But direct I/O will be for the disk, but if you have to pull from the network, right, it's got to go through the kernel.

**Simon** [34:46]:

Yeah, we're doing the copy right now. It hasn't been a bottleneck yet.

**Audience Member** [34:49]:

Oh, okay, okay. And then how large are your block sizes?

**Simon** [34:53]:

The block sizes, I believe right now they're around 128k.

**Audience Member** [34:57]:

Okay.

**Simon** [34:57]:

To balance it between the NVMe SSD and S3, you could imagine a system where you have different block sizes, but I think that's what it is. It might, they might have changed it.

**Audience Member** [35:06]:

Right. It's kilobytes, not megabytes.

**Simon** [35:09]:

Yeah, exactly.

**Audience Member** [35:10]:

Got it. All right, thanks.

**Simon** [35:11]:

On the right here, we can see that the cold performance is sort of like sub-second still as we go directly to object storage, and then the warm namespaces are just as fast as any other system. If you look at our production traces, you'll see sort of like maybe a millisecond or less actually servicing the query, and in 10 milliseconds going to S3 to make sure we have the latest entry from the WAL. So you can turn that off for other characteristics, but we think this is a really good default for search systems.

**Simon** [36:00]:

The reason why fast cold queries are so important is because if, let's say that our cold queries are 10 seconds, you end up just patching that everywhere in the system, right? You end up pinning more stuff into the cache, you end up having larger caches, and this causes poor economics to pass on to our customers. So it's very important to have fast cold queries for everything in the system to not be so fragile. The other thing in V1 was that in 2023 when I was writing the first version of turbopuffer, these HNSW or graph indexes for vectors were all the rage.

**Simon** [36:40]:

The problem with them to me is that they have a lot of dependencies, right? They're very difficult to speculate on. Connecting to the point before of modern hardware, if you, the simplest way to do a vector search is basically just to have a query vector and then compare it against every single vector in the dataset and then return the 10 closest ones by the angle that they have to each other. The most popular algorithms at the time were to essentially construct a nearest neighbor graph of this. You can imagine these vectors in two dimensions, right, as they are here, and you try to connect the vectors that are closer to each other in a graph.

**Simon** [37:20]:

Then you drop yourself at some point in the graph, and then you just kind of do a greedy search along it to find the closest vectors to your query vector. The problem with a graph is that every time you navigate between these edges, you can't really predict how far you're going to navigate and speculate and get all of this stuff, look page in from disk. So you end up doing all of these dependencies. And so if you're an object store, it's like 100 milliseconds, 100 milliseconds, 100 milliseconds every time you're walking the graph on disk, right? And a random read is in the hundreds of microseconds, which is still comparatively quite slow.

**Simon** [38:00]:

A clustered index is the most sort of traditional approach where you just take the vectors and you divide them into clusters. You try to find some underlying pattern, and then you average the vectors in the cluster and take the average, and you have what's called a centroid. When you're doing a search, you just compare your query vector against the cluster centroids and then only fetch the few clusters that are closest to your query and then exhaustively scan through those to get your top results. This has two round trips, right? You can imagine a cluster1.bin, cluster2.bin, and you just download the appropriate data.

**Simon** [38:40]:

That is not very far off from how it worked in the first version of turbopuffer. This is fast on disk, and it's fast on object storage, and it's very, very simple to implement. Caching has also not changed very much from the very first version of turbopuffer. The NVMe SSD is a simple cache tier. We just use a FIFO ring buffer where you just continue to write, write, write, write, write. At some point, you wrap around, and you start eating the tail. You can do a variety of different interesting techniques on top of this to make sure that values that should not be evicted from the disk cache are not.

**Simon** [39:20]:

Right now in turbopuffer, it's very simple. Complexity, we think, really has to be deserved, and right now this is working quite well. This is a read-through cache, right? So different objects are just, well, it's actually both read-through and write-through, right? At write time, we'll write into it, and then when we read an object from object storage or lower in the hierarchy, then we'll populate it up. The cache is prioritized, so namespaces that are really busy will prioritize warming over others, and cache efficiency is a pretty heavy investment area for us.

**Simon** [40:00]:

This is also where you get to control, not separating compute and storage. This is what I call compute-storage flexible, because there's no reason why you can't tell the cache to always keep something in cache if you want to make sure that your p99 is very low over very long periods of time. V1 sort of reached its logical limits in the middle of 2024. The first version of turbopuffer was literally, as the caching layer, a caching NGINX in front of S3. When I brought Justine, my co-founder, along, this was obviously the first thing that we fixed, but hopefully it just goes to illustrate how simple we've iterated on.

**Simon** [40:40]:

Every single part of turbopuffer is the simplest, most reliable thing that we know will work. I have operated NGINX for more than a decade, so I know it works, I know how to configure it, and every single layer of complexity we add on has to be deserved. FTS, like full-text search, was bolted on, but it was very limited as the more general-purpose database. I believe that any good database, any large database over time ends up implementing every query plan. And so it has to be designed as such, otherwise you'll be pigeonholed into a single workload.

**Simon** [41:20]:

When people have, when your customers have commercial attraction on your database, they will expect more and more and more types of queries to execute on it as they trust it and put more and more data on it over time. V1 taken to its logical extreme was what found us product-market fit, and also it was what ran Cursor, Notion, and also got us the POC with Anthropic and won that towards the end of 2024. While we were working on all of this, we were also in parallel, we'd hired Boyan.

**Simon** [42:00]:

And Boyan, actually, who I met at I.O.I. in 2012, actually knew how to write databases. And so in parallel of primarily Justine and also myself optimizing the V1, we were writing a proper V2 version of the database that we're now on. We now joke that there's still remnants of V1 in the code base, and Morgan calls this an important company objective to remove all of the founder code in the code base. So before we go to V2, I want to justify the existence of a new type of database, right? The title of this series of talks is why not just use Postgres for everything?

**Simon** [42:40]:

There's two things that I keep coming back to. The first one is if you want to build a large, commercially viable database, I think that you basically need to have the propensity that every single company on earth either indirectly or directly is going to have data in your database. If I think about even the coffee shop around the corner from my house, I can guarantee you that they have some of their data somewhere in an Oracle database, right? It's in a Snowflake database; it's in Databricks. The biggest database companies in the world have a little bit from every single company on earth.

**Simon** [43:20]:

If you want to build a big database company, then I think that has to be part of your vision. And the best time to do that is when there's some underlying new workload or paradigm shift that causes that to happen, right? For Snowflake and Databricks, suddenly we have these websites with lots of traffic, and we wanted to do large-scale analytics on it. Like fundamentally new economics, Oracle, Postgres, MySQL, and others came before it because, hey, great, we have computers; let's put lots of data in them and make it possible to query.

**Simon** [44:00]:

These are not generation one, two, and three in terms of one being better than the other; all of these are different phenomenal trade-offs, and turbopuffer is just another set of trade-offs that happens to work very well for the new workload right now, which is to connect very large amounts of unstructured data to LLMs. That's the workload that's causing traction for turbopuffer. The second thing that you need is that you need some new storage architecture that the incumbents can't easily do. If you only needed one, then you would sort of imagine that in 2013-ish, as everyone started creating mobile apps, a big company called MongoDB would have spun up and dominated the world because everyone has geo-coordinates in them.

**Simon** [44:50]:

But there's no reason why geo-coordinates can't be supported in the Gen 1 and Gen 2 databases. So the new storage architecture that we can create now is what I call compute-storage flexible or object storage native, which was enabled by these developments, and we can now create a database that has these trade-offs that are quite phenomenal for this new workload. These are the new attributes, and this is what would be very difficult to pull off inside of Postgres. Now, of course, inside of Postgres, you can make computers do anything in the limit, but there has to be some pragmatic trade-off of when you do this.

**Simon** [45:30]:

V2, so V2 of turbopuffer has a bunch of evolution on top of the ideas that were good in V1. The biggest change from V1 to V2 is that V1 you can think of as operating almost more or less directly on very simple files on object storage, whereas V2 is introducing a key space that's maintained by an LSM to implement all these different data structures on top. The other big change was to do incremental indexing. The hilarious thing about V1 was that once the WAL had progressed enough and we had to rebuild the index, we rebuilt the entire WAL. The quote-unquote compute amplification on this was enormous, but it was very simple, and it was very reliable to maintain.

**Simon** [46:20]:

These two also had things like branching and our own load balancer. So we'll go through these in a little bit more detail. The LSM, by academic standards, is like very disappointing. It's really a LSM 101 implementation. The system side of the LSM has been heavily optimized for performance, but in terms of compaction, it's more or less the simplest tiered compaction that you can imagine. We expect to spend a lot more time here as we find this to start becoming a bottleneck for new workloads, but the LSM is very simple.

**Simon** [47:00]:

If there's anyone in this room that's considering what to research at some point, I think the compaction territory that we're in with these object storage native databases is quite a different set of trade-offs than a traditional database, right? On a traditional database, you might, on a shard, you have to contend with the fact that you can't spend too much compute, too much I/O, too much memory trying to compact the data because you're contending for the same resources as you are when servicing the queries. In this case, you can, you can, you know, spin up a dedicated index node for a very short time, run compaction, and build indexes on it as well.

**Simon** [47:40]:

This has a different set of trade-offs, right? Space amplification is very cheap because we're using S3. You have to deal with this new type of amplification on the number of S3 operations that you're doing. Write amplification is cheap; read amplification is more expensive. Round trips matter, and throughput matters a little bit less. So this is a different set of trade-offs. And as far as we know, there's no research on doing compaction in this kind of environment.

**Simon** [48:10]:

The other thing that we did in V2 was make the vector indexing incremental. Again, somewhat hilariously, in V1, the vector indexing was not incremental. We rebuilt the world once the WAL had progressed enough. SPFresh is essentially a paper that talks about how incrementally to maintain clusters. If you do clustering, you can imagine you do a bunch of like very simple repeated multiplications and distancing to find the right clusters, and then you have the clusters. You can also imagine you have an intuition that, well, you should be able to maintain them, right? Like you insert a new vector into a cluster, and you expand the cluster.

**Simon** [48:50]:

But at some point, the cluster becomes large, and you have tail latency. Well, so if the cluster grows very large, you could split the cluster, or if you're removing vectors from a cluster, you could merge it with adjacent clusters. Now the problem is that that causes the clusters nearby to also potentially change because if you're splitting or you are merging clusters, vectors that were previously in those clusters might now better belong to other clusters. This is like a concurrency and implementation nightmare, as you can probably imagine. But this works unbelievably well at very large scale with a very large amount of throughput of indexing, and the performance characteristics on this on disk and object storage is really good.

**Simon** [49:30]:

We've had this in production now for almost a year, a year and a half, and we're now working on the new version of this that will be even more powerful, helping us integrate everything that we've learned about running this. This is certainly part of the core parts of turbopuffer. The other thing that we do is hierarchical clustering. So you can imagine that if you have a very large vector index that's like, say, a billion vectors large, scanning through all those centroids to find the closest clusters becomes a lot of compute. So we create clusters of clusters.

**Simon** [50:00]:

So essentially, we're creating a tree. And the other thing that we do is that we quantize, so basically take these big float F16 vectors and get them to be just binary vectors. We can do very, very simple operations on them. And then this algorithm called RaBitQ, which we use to quantize, gives us an error bound on which ones we have to re-rank with the full fidelity vector to get some recall target, which is to get sort of the, you know, these are all approximate algorithms, so we have to have high recall, which is sort of the percentage overlap with the exact ANN result.

**Simon** [50:50]:

So these are also some of the things that we've done to support some of these use cases where people are ingesting all of Common Crawl or other very, very large datasets into turbopuffer. We wrote a blog post about how we did this, and this is how we can search around 100 billion vectors with a p99 of 200 milliseconds. It's probably been brought down by then, but we've never heard of a result of that scale. Filtering is a big challenge with vector indexes, and I think a lot of people somewhat irresponsibly just slap a vector index onto their existing query planner.

**Simon** [51:30]:

The query planner has to be very aware of the fact that the clusters that are matching something might be very far away, and that can create all kinds of problems in planning the query where you don't actually get the closest vectors. Not going to go into a ton of detail on this here. We have a blog post with a little bit more, but just something that the query planner certainly has to be aware of the distribution of the filters with the vector data to plan a query with high recall.

**Simon** [52:00]:

We have three more indexes to go here. The full-text search index in turbopuffer is fairly sophisticated, and this is drawing a lot of inspiration from Lucene. We're starting to see comparable performance and even beating performance from Lucene on very large datasets for very long queries, which are the ones that we're increasingly seeing in production from LLMs increasingly writing the queries. Essentially, when you're doing full-text search indexing, you're splitting the corpus by, say, space or something like that, and then you're putting all of that into a map, and then you have a set of all of the documents that match that.

**Simon** [52:40]:

You also have to do all this stuff around scoring, and that's where we store with every block of documents, we store the maximum contribution to the full-text search score of the documents inside of that block. What that allows us to do is an enormous amount of skipping. And so you can imagine that for something like if you're searching for a term like "puffer," it's a lot more rare than a term like "the." And so the score in that block is a lot higher. We can use that further in the next slide where I'll talk about the algorithm we've used for skipping.

**Simon** [53:10]:

But we can use this information to rip through these postings and intersect them. Full-text search really just boils down to intersecting very, very large posting lists. And so it really becomes fundamentally a bandwidth problem. To use these posting lists, the very simple observation is that if you're searching for something like "New York population," you can imagine that there is a point where it just does not make sense to look at any documents that have "new" in them anymore. Like basically any document needs to have "population" in "York" to actually be viable for the query, and then we can stop intersecting the new list.

**Simon** [53:50]:

This algorithm is called max score, and it performs really, really well on modern CPUs. Recently, sort of the trade-off switched to be in favor of max score over wand, so what this boils down to is a lot of complexity in search engines is around skipping documents for ranking that no longer matter, and this is the one that we've implemented in turbopuffer. It's also the one that's in Lucene, and I think these are the only two implementations of it. Regex queries are another type of text query. The way that you service these is that you essentially break them and break the, you parse the regex into an AST, you break it into trigrams, and then you post-process on all the documents that match the trigrams that are definitely going to be in the documents that match.

**Simon** [54:40]:

This essentially just boils down to a full-text search query, but the number of trigrams is finite, so we can just index some numbers and save a bunch of efficiency here. So if you're searching for something like "main" and "util," we break it out into the trigrams, right? And there's going to be some regexes that are very difficult to compile into trigrams, and that's when you have to run the final regex on the document. This is very common for code. And then the last index that we have is the filtering index, so exact filtering. This has to work congruently with the vector queries and full-text search queries, and the planner has to be aware of when it's better to filter versus score because sometimes it's better to score and then filter, and other times it's better to filter first.

**Simon** [55:30]:

In a lot of ways, it shares a lot of sort of conceptual logic with the inverted index of use of full-text search, and underneath it's a simpler bitmap implementation that figures out what filters match into the heap. I won't go into too much detail on this, but essentially we've also written a queue on top of object storage to avoid having another dependency. This is not for the WAL or ingestion of data, but really just to notify the indexes of when to compact and do various maintenance operations. It's essentially just a single file called q.json that we do a lot of CAS on, and it goes through a broker that's doing group commits on this file with all the traffic from lots of query nodes. And this works great. It's a very simple design.

**Simon** [56:10]:

Again, we only introduce complexity when it's deserved. The last thing that's coming to turbopuffer very soon is, which is easier on an object storage native type of databases, branching. So basically the ability at a particular LSN or WAL entry to branch off and then start a completely new timeline and then have references back to the previous branch. This is common in coding, it's common for development, it's common for testing, and lots of other use cases. This is something that's coming to turbopuffer very, very soon.

**Simon** [56:50]:

Final slide here is that what's next for turbopuffer is to implement more and more and more query plans. There comes a time where you have to steer clear of the uncanny valley of continuing to use JSON syntax and actually give people SQL. I don't know when we're at that threshold, but I think we'll know it when we see it. Faster, of course, and then continue to iterate on caching and lots and lots of other features like branching that I just mentioned. I'll leave it there for questions. Thank you so much.

**Andy** [54:37]:

All right, that was awesome. I will clap on behalf of everyone. So if you have a question for Simon, just unmute yourself and go for it. And I know there's a question in the chat that I'll read out later on afterwards.

**Audience Member** [54:51]:

Hi there. Thank you. Oh, I'm sorry. You can go ahead first.

**Andy** [54:56]:

Okay. Yeah, so I just had a quick question if you could clarify my understanding of the caching. So you said it was a read-through and write-through cache. Let's say I have a search on Postgres, and then it's cached, and I write a document, like auto-tune Postgres autovacuum, and then I read that again. Is it going to get that document then right away, or how does that work?

**Simon** [55:20]:

Sorry, what's the Postgres connection here?

**Andy** [55:22]:

No, I'm saying if you have a search that's already cached on the term Postgres and then you write a document with Postgres in it, are you going to get that right away, or how does that work?

**Simon** [55:33]:

Yeah, so it will go in the WAL, right? And the WAL would be written into the NVMe cache. I don't think we write it to the memory cache too. And so when you're doing the query, right, we will immediately see it because it's in cache, and we're doing the round trip to the WAL to make sure that we see the latest entry. So it would be immediately visible and written into the cache because otherwise, every time you write a document, we'd have to go to object storage to fetch it.

**Andy** [55:58]:

I thought, but I thought you said you didn't acknowledge the write until it made it to object storage, no?

**Simon** [56:02]:

Yeah, I mean like you're not gonna see it until the client has returned, like the client sees the 200.

**Andy** [56:09]:

Okay, got it. Thank you.

**Simon** [56:10]:

Sorry, is it clear?

**Andy** [56:11]:

Yeah, no, that makes sense. Thank you.

**Audience Member** [56:12]:

Hey, thank you so much. Appreciate this talk. I had a question, which is what, if anything, have you noticed while building turbopuffer in terms of the difference between storing information like common text files versus code? And where have you seen like the most interesting differences?

**Simon** [56:45]:

Trying to think of a simple answer here. I think code is like extremely sharded, right? There's so many different shards from all of the companies that we have that use code. And the write-to-read ratio is very high. There's a lot more writes than there are reads. I think in other systems where it's simpler text, the queries often get more complicated. So in code, the queries are usually quite simple, like find this regex, whatever. There's generally very little filtering, whereas for text in a more of like in an Atlassian or like Notion use case or something like that, there's a lot more filtering, often a lot of permissions. I did not know how much time I would spend on optimizing or that we would spend on optimizing query plans for permissions, which are very, very large intersects and are very well-suited for posting lists over B-trees. So I would say those are some of the first that come to mind.

**Audience Member** [57:52]:

So thank you for this talk. I had a question. If you were not building turbopuffer, what would you be building in like Gen 3? Kind of databases, you know, the object native databases? Or to reframe it, like if you have to build another database company, what are the other things you would build?

**Simon** [58:20]:

This is a phenomenal question. Okay, so I spent a fair amount of time thinking about this. There's not that many companies to be built, unfortunately. There's a lot of databases to be built, but I don't think that many of them will turn into big companies. One was the streaming use case, right? So WarpStream and others have essentially done that. I think there were companies to be built there. They are not quite able to be propelled by the new workload in the same way, but it is just the superior architecture for streaming. The second category is search; that's the one we're in here. The third category is sort of real-time analytics. I think there's a bunch of different vendors that are doing that. It's not a category I would go straight for; it's a very contentious category.

**Simon** [59:10]:

The other category is observability. Almost every observability provider builds their own database in some way and then slaps a big UI on it, but it's not a place where you would actually be selling the database. You have to be selling like a full end-to-end experience. If I had to choose to build in another vertical, it would probably be there and try to do it not at the 85% gross margin of the incumbent, but more like a 60 or 50% gross margin and try to expand from there. But that would be very capital intensive, and it requires an enormous amount of UI work to pull off successfully. The other category that I think is interesting, other than the ones that I've just mentioned for this database design, is that there's lots of companies that can navigate a very specific set of trade-offs and create their own database a lot easier now than they would have been able to in the past.

**Simon** [1:00:10]:

This is probably where the majority of these databases are going to be built inside of companies that have a particular set of trade-offs, but that is not a generally viable large database company. That is my somewhat pessimistic answer to your question.

**Audience Member** [1:00:12]:

Thank you so much.

**Andy** [1:00:17]:

Okay, go for it.

**Audience Member** [1:00:19]:

Oh, thank you.

**Audience Member** [1:00:21]:

So I'm curious about the consistency model within like a single namespace. My understanding is that like with inverted file indexes updated like asynchronously after you have like a write-a-log like commit. So if there's like concurrent write and reads, is there like a risk of like getting like a mixed state where like some documents are like indexed like wrongly and like others like updated in time? And is this like a trade-off that's like acceptable in the targeted workloads? And like, do you see cases where like this could be like a real problem?

**Simon** [1:00:58]:

Okay, so just quickly to reiterate how we did the search thing, right? So if you're doing full-text search, if you're doing vector search, it's actually easier. So let's say that the WAL is like 100 MB behind, let's use a nicer number, 64 MB ahead of the index, right? Then what we'll do is we'll do, we'll go to the index, right? Consult the clusters, do that search, and then we'll replay the WAL on top, right? Well, we kind of do those two operations in parallel.

**Simon** [1:01:30]:

And then you have a consistent result. I think what you may be talking about is a situation where you might be updating a vector, and then in my second slide, right, I show how you can do some simple conditions and things like that, right? And so you could do a patch by filter, things like that. That requires running a query and then doing the update, and in between those events, it can be possible, right, for something else to occur. So you'd have to rewrite the operation, right? turbopuffer is at a read committed isolation level, so that is the kind of thing that could occur, and where having multiple writers could, yeah, that's I think fairly common in databases and other systems and felt like the right trade-off for turbopuffer.

**Audience Member** [1:02:07]:

Thank you. I also have like a different question; it's more like related to business. So given like the querying for structure like S3 and GCS, like they're all owned by like hyperscalers, and they could theoretically build like something similar to investing like enough time and resources. So what do you think your motive?

**Simon** [1:02:28]:

Okay, so two things. Working with a hyperscaler is just something that every database company contends with, right? It's just the rule of the law, and inside of these large companies, the team that works directly with you is incentivized to have you grow as much as possible. And there's another team that's incentivized to grow their database as much as possible. And just these organizations like that works perfectly, and it's just like the incentives are there aligned, and I think that's well and good with all of the vendors. But of course, if they see a lot of commercial traction, they have the signals to try to build something similar.

**Simon** [1:03:12]:

And at least one of the clouds has done something like that that looks a lot like what turbopuffer looks like.

**Audience Member** [1:03:14]:

FPGAs, right?

**Simon** [1:03:14]:

Correct. But it doesn't have the cache hierarchy and things like that in front, right? So if you're building a database company and you're competing with the hyperscalers, you have to think about what you can do that they could never do, right? One of the things the hyperscalers could never do is to put the database engineers into a Slack channel with the companies that actually matter and have them just work directly on the query plans.

**Simon** [1:03:40]:

Another thing that's very difficult to do at that scale is to manage all the caching and the multi-tenancy and things like that, and then also releasing features at some particular cadence. I think a startup's only moat is focus; that's it. And so you can focus on something for a long period of time, I think you will do really well against the hyperscalers. If you think about, for example, even the Postgres offerings of the hyperscalers, most of them are not even on NVMe SSDs, even though those were released almost a decade ago on cloud SKUs. So there's lots of opportunities, I think, for database companies to even just run these databases better than the clouds are themselves.

**Audience Member** [1:04:15]:

I'll just point out you built V1 in a cabin in Canada and launched it, and we're not going to any design meetings with the team, right? So, like, you can't do that at a big, at a larger scale.

**Simon** [1:04:27]:

Yeah. No, no, no, you could not. You could not. Like that.

**Audience Member** [1:04:31]:

Yeah, of course.

**Simon** [1:04:32]:

V1 uniquely works, right, because you can be very close to the customer, right? And so like, if there's anything that comes up in a POC, you can give them an experience of trying to fix that immediately, right? I think maybe another way to phrase that is that when Notion became a turbopuffer customer, they were using every single line of code in the database, right? And that's an advantage that you have where you can launch a lot quicker.

**Andy** [1:04:56]:

All right. So we're almost out of time, as I appreciate you going over. First question would be. It sounds like all your file formats running S3 are all proprietary or custom stuff that you guys developed. At any point in the design or the building out of either V1 or V2, did you consider using one of the open-source file formats like Parquet, ORC, and the newer ones (e.g. Nimble)?

**Simon** [1:05:19]:

The main reason that we didn't go that route was because of speed. We just want to control. There's no philosophical debate. If everything could be in the customer's bucket in an open file format and we could move at the same pace, I would say that would be a superior design. But again, a startup's only moat, a startup's only thing that it has on everyone else is speed. And so we've always optimized everything for what is most reliable. Same reasons we're not open source; we're not like we're only commercial, not because we don't think that open source is great, but because we don't have time.

**Andy** [1:05:58]:

Yeah, sure. And then the last one, I think you mentioned you had a backup slide of your object storage bag of tricks. I know we're over time. Could you share that?

**Simon** [1:06:08]:

Yeah, we will turn this into a full-fledged blog post at some point, but there's a lot of different tricks that you have to employ when you're building for a turbopuffer or an object storage native database, right? So just to take a couple random ones, speculation is a big one, right? Like if you, you're always going to want to try to figure out or speculate what the next round trip is going to be and then do it in parallel with whatever you're doing. We use this trick everywhere. If you see the traces from turbopuffer, just constantly speculating on what we might need next.

**Simon** [1:06:50]:

The metadata layer for S3 and GCS and every all the other ones is much faster than the storage layer, so fetching like a 4 kB metadata file is maybe like has a p99 of say like 50 milliseconds, whereas just getting the metadata of is this the one I have locally, the same game might have a p99 of closer to 30 milliseconds depending on the provider. The P50 is generally a lot lower for the metadata, but when we design, we always design for the p99. Another random one we talked about list earlier; one of the other things that we do is that we keep a read-through cache of a bunch of the starting points in the bucket of their places to start from, so we can parallelize and minimize the list.

**Simon** [1:07:48]:

We run thousands of concurrent list calls and know where to paginate from until they overlap. 404s are free on GCS; you can do crazy things with this if you really consider it. They're not free on any other provider, but if you're only on GCS, this is very interesting.
