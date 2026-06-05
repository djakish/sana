# turbopuffer on Postgres FM

September 12, 2025•Postgres FM

[Video 3](https://www.youtube.com/watch?v=dgWmmbr_Fk8)

## Transcript

**Nikolay Samokhvalov** [0:00]:

Hello hello, this is Postgres FM. As usual, my name is Nikolay Samokhvalov and my co-host is Michael Christofides. Hi, Michael.

**Michael Christofides** [0:08]:

Hi, Nikolay.

**Nikolay Samokhvalov** [0:09]:

And we have our unexpected guest today, Simon Eskildsen, CEO and co-founder of turbopuffer. Hi, Simon.

**Simon Eskildsen** [0:18]:

Thank you for having me.

**Nikolay Samokhvalov** [0:19]:

Yeah, thank you for coming. It was very unexpected because we mentioned turbopuffer last time and you messaged me on Twitter. I think it's a great idea sometimes to look outside of traditional PostgreSQL.

**Simon Eskildsen** [0:32]:

Ecosystem.

**Nikolay Samokhvalov** [0:34]:

I think it's beneficial for everyone. Should be. So yeah, thank you for coming. For sure it's a great idea, I think.

**Simon Eskildsen** [0:41]:

Yeah, the origin story is kind of funny, and it was only a couple of days ago. I have a script where if turbopuffer is mentioned anywhere, then I'll get an email or a summary. You guys were discussing last time different ANN technologies, both in Postgres and outside, and turbopuffer was mentioned. So I just DM'd you and asked, "Hey, can I come on the channel?"

**Nikolay Samokhvalov** [1:00]:

Yeah.

**Simon Eskildsen** [1:00]:

Chat about Postgres, chat about MySQL, chat about databases and when you choose one over the other, and when Postgres breaks for some of these workloads that we've seen and when it's great. And now it's what? Yeah, two or three days later and we're on.

**Nikolay Samokhvalov** [1:13]:

Including the weekend actually, so the podcast was out on Friday and we recorded this on Monday. That's a velocity everyone should try to achieve. I like it.

**Simon Eskildsen** [1:23]:

Yeah, and you've had chicks hatch in the meantime.

**Nikolay Samokhvalov** [1:26]:

Oh yeah. That's why my camera is overexposed because the whole night this camera was used to broadcast. I didn't have time to tune it properly. But again, thank you for coming. This is about databases. This time probably not so much about Postgres, but definitely we should talk about vectors, right? And maybe we should start from distance and talk about your background.

**Simon Eskildsen** [1:52]:

And I've heard some MySQL is involved, right? Can you discuss it a little bit?

**Simon Eskildsen** [1:57]:

For sure. Yeah, my background is I spent almost a decade at Shopify, scaling mainly the database layer there, but pretty much anything that would break as the scale increased through the 2010s. Shopify, like most companies, started in the early 2000s on MySQL. So a lot of the work that I did was with MySQL, but also every other database that Shopify employed like Redis, Memcache, Elasticsearch, and a ton of others, Kafka, and so on. I spent a long time there. When I joined, it was a couple hundred requests per second, and when I left it was into millions. I was very intimate with the data layer there. I was on the last resort pager for many years, and that has informed a lot about how I write software today. A couple of years ago, I started a company called turbopuffer because...

**Nikolay Samokhvalov** [2:46]:

Before...

**Simon Eskildsen** [2:46]:

I thought...

**Nikolay Samokhvalov** [2:47]:

Sorry for interrupting. I remember Shopify actually. We always command the article. Should have had a couple of blog posts about UUIDs and how your UUID version four is not good and version seven is much better. It doesn't matter, MySQL or Postgres, the mechanics behind the scenes are the same. You can see how B-trees behave. I remember, and I think, Michael, we mentioned that article on the podcast a few times as well.

**Michael Christofides** [3:14]:

We had mentioned it, Nikolay, for sure, and I think Shopify has come up before. I thought it was a Vitess shop; I thought it might have been one of the early Vitess adopters.

**Simon Eskildsen** [3:23]:

Shopify is using a little bit of Vitess, but not very much. Vitess was not around when we did all the sharding back in the early 2010s, so we did it all at the application layer. I wasn't part of the actual sharding decision, but I was part of a lot of the sharding work over the years. It's funny because at the time I know that they looked at a bunch of proxies and all of the businesses they later looked at had gone out of business. It's not a great business, unfortunately. Everything was done in Rubyland through a module called sharding. It did a lot of things and a lot of monkey patches into Rails. But let's talk about this GUID v4 thing because I think...

**Michael Christofides** [3:58]:

Yeah.

**Simon Eskildsen** [3:58]:

If we wanted to do a pros and cons, MySQL versus PostgreSQL, I spent quite a bit of time with both. This one actually, to my knowledge, only really matters for MySQL. Well, it actually matters for PostgreSQL as well, but in a different way. So on MySQL, right, the primary key dictates how the B-tree is laid out for the primary key, right? So for the entire row. If you have UUID v4, it's completely randomly scattered along the B-tree. Whenever you're doing an insert, it will just kind of fall somewhere random in the B-tree, which makes the updates very expensive. If you're adding ten rows at a time, doing ten insertions in ten different leaves, you're doing a lot more disk I/O and your write amplification is high. In PostgreSQL, of course, you're just appending to the heap with all of its drawbacks and benefits, and it doesn't matter as much other than on the indexes, right? To my knowledge, on the indexes it will matter a lot because on the indexes, of course, it is sorted by that, and if you have some temporal locality, then it's not going to matter as much. So that's my understanding. This matters a lot in MySQL. Now, that article I think was after I left, and Shopify doesn't use UUIDs as primary keys for anything, so I don't really know where this mattered. It must be something tangential because MySQL really just does auto-increment, and every shard does an auto-increment with basically like a 32k auto-increment number, and then every shard has a plus offset into that to allow it to grow to 32k shards. Given how much of a pain it would be to change that, that's probably still the case. But I always really liked that scheme. Some tables over time at Shopify ended up having a primary key on the Shopify ID and the ID because that would give locality for a shop. Otherwise, you have a lot of random I/O if you're trying to dump out a bunch of products for a shop because the chance that there are going to be multiple products for a shop in a leaf unless you do that is just a lot lower. So that ended up working really well. Oh, and this is a pain to do in PostgreSQL because if you want to rewrite the primary key or the heap by an ID, you have to rewrite the entire thing. That was one of my surprises having worked a bit more with PostgreSQL in later years.

**Nikolay Samokhvalov** [6:14]:

Yeah, yeah, yeah. I agree with you, and I agree that in PostgreSQL it also matters, but only for the B-tree itself, primary key B-tree. If it's auto-increment or in PostgreSQL it's called bigserial, for example, or auto-generated. Right now, there's another method, but behind the scenes, it's also like sequencing and inserts. Oh no, in this case, it's not sequence; there should be a function that generates UUID version four. If it's random, like version four is random, version seven is closer to regular numbers basically, right? Because it's monotonically growing, right? Lexicographically ordered, right? So in this case, you insert only on the right side of the tree, and dirty pages, if you think about how the checkpointing is working, also there's in PostgreSQL, there is also an overhead after each checkpoint is full page, right? Which involves indexes as well. So if you touch random pages all the time, a disk I/O overhead and replication and backups actually everything receives additional overhead. While in version seven, we write on the right side all the time; it's much better. But the heap—yes, the heap is different. So I agree with this. Anyway, I just wanted to say we use the MySQL article because it's written very well, and in PostgreSQL, we didn't have a version seven for quite some time. Last Thursday, version eighteen release candidate was out, which will include full implementation of UUID version seven, which was live coded on PostgreSQL with a couple of friends just in Cursor, I think. Oh no, it was not in Cursor; it was before that. But anyway, it was just created right online with—we did it, and right now it took a couple of years to reach maturity because PostgreSQL always waits until RFCs are finalized and so on. Anyway, soon version eighteen will be out, and UUID version seven is in sight. But I think everyone is already using version eight on the client. Okay, great. So you had this great career and then decided to create another—is it a database system, database management system?

**Simon Eskildsen** [8:37]:

It's certainly a full-blown database. Underneath turbopuffer is an LSM tree, and LSM works really well for object storage. You know, every successful database ends up implementing every query eventually, right? In the limit, turbopuffer will end up doing the same thing. But every good database also starts with some specialization, right? Our specialization has been on search workloads. I would say that it's by no means a replacement for PostgreSQL. There always comes a time where it starts to make sense to move parts of data into more specialized algorithms, more specialized data structures, and more specialized storage. In general, my hypothesis on when it's time to create a database is that you need two things need to be true in order to create a generational database company. The first thing that you need is a new storage architecture. Because if you don't have a new storage architecture, some new way to store the data, ideally both data structure-wise and also the actual medium that you're persisting on, there's no reason why in the limit the other databases won't do the workload better. They already have the existing momentum; they already have the work of the workloads. In the PostgreSQL case, of course, you know it's the classic relational architecture where you replicate every byte onto three disks, and...

**Michael Christofides** [9:52]:

Mm-hmm.

**Simon Eskildsen** [9:53]:

This works phenomenally well. Right? We've had this in production for probably more than four years, and it works great. It has high performance; it has a very predictable performance profile, and it works really, really well with the page cache or the buffer pool, whatever database you're using. The problem with this model is that if the data is not very valuable, this model is expensive. Every gigabyte of network-attached disk is about ten cents per gigabyte. Unless you're a really risky DBA, you're gonna run those disks at fifty percent utilization on all the replicas and on the primary. So you're paying for this disk kind of three times, which ends up with an all-in cost of sixty cents per gigabyte. That's not even accounting for all the CPUs that you also need on the replicas because you need the replicas to process the writes as fast as the primary, so you're kind of paying for the same thing three times. The all-in sort of per terabyte cost, when you also take into consideration the disk systems, can be a little bit more memory-hungry, is probably around sixty cents to two dollars per gigabyte.

**Michael Christofides** [10:54]:

Per month, right?

**Simon Eskildsen** [10:56]:

Per month, yeah, per month USD. On object storage, the base cost is two cents per gigabyte, right? When we need that data in RAM or on disk, we only have to pay for one, and we only have to pay for some percentage of the data to be in cache at all times. You mentioned Cursor earlier; Cursor is a turbopuffer customer, and they don't need every single code base on SSDs or in memory at all times. They need some percentage in memory, the ones that are queried a lot right now, and some percentage on disk, the ones that are gonna be queried again in a few minutes or maybe in a few hours. You end up paying a lot less because we only have to keep one copy of a subset of the data rather than three copies of all of the data. Now that comes with a fundamental set of trade-offs, right? We want to be upfront about that; you can't use turbopuffer for everything. If you want to do a write to turbopuffer, we commit that to S3, right? By the time it's committed to turbopuffer, the guarantee is actually stronger than most relational systems because we've committed it into S3, which takes a couple hundred milliseconds, but the durability guarantee is very strong. If you're building a system like Shopify, well, you can't live with a commit time in the hundreds of milliseconds; it's just not acceptable. So that's a trade-off that means that this is not a system that will ever replace a relational database store. The other downside is that because not all the data is on disk or in memory at all times, it means that you can have tail latency. That can be really catastrophic in very large systems that are doing millions of queries per second. If you can't rely on a very predictable query profile, you can have massive outages to hydrate the caches. I've seen these outages on disk; I've seen them, and just even the workload changing slightly can mess with the buffer pool in a way where you have a massive outage. These two things may sound like small trade-offs, but they're massive trade-offs for very large production systems. But it might mean that if you have, let's say, a billion vectors and you're trying to store them into PostgreSQL, the economics just don't make sense. You're paying thousands and thousands, if not tens of thousands of dollars in hardware costs for a workload that might cost tens or hundreds of dollars on turbopuffer.

**Michael Christofides** [12:56]:

How much is it really, one billion vectors if one vector is what's like, kilobytes?

**Simon Eskildsen** [13:02]:

Seven hundred and sixty-eight dimensions.

**Michael Christofides** [13:06]:

Yeah.

**Nikolay Samokhvalov** [13:07]:

Oh.

**Simon Eskildsen** [13:07]:

It's...

**Michael Christofides** [13:08]:

Wow.

**Simon Eskildsen** [13:08]:

Um...

**Michael Christofides** [13:08]:

Three kilobytes each.

**Simon Eskildsen** [13:09]:

It's three terabytes.

**Michael Christofides** [13:12]:

Three terabytes to store one billion vectors.

**Simon Eskildsen** [13:14]:

Yeah.

**Michael Christofides** [13:14]:

And also don't have a good index for one billion scale. Yeah. I mean, for PostgreSQL, each vector, HNSW won't work with one billion.

**Simon Eskildsen** [13:25]:

I mean...

**Michael Christofides** [13:26]:

But...

**Simon Eskildsen** [13:27]:

We could just run the math a couple of different ways, right? I'm not saying this is how it works in pgvector; I'm less familiar with it now. But even if you have three terabytes of raw data, you're probably going to need to store more than that. You might be able to do some tricks to make the vector smaller, so you only have to store maybe, I don't know, a terabyte or something along those lines, right? But remember that a gigabyte of DRAM is five dollars per month, and you need that three times on all your replicas. So you're paying fifteen dollars per gigabyte per month. So if you're doing that, if you have to store that three times, you put everything in memory, and you're somehow able to get it down to a terabyte, then you're talking about fifteen thousand dollars per month, right, across the three replicas, just for the RAM alone.

**Michael Christofides** [14:08]:

Mm-hmm.

**Simon Eskildsen** [14:08]:

Yeah, I agree with you. It shows that memory is the key, and the creation of an index takes a lot of time. For a billion, I already have issues with a few million vectors scale. I know Timescale, which has now renamed to Tiger Data; they developed another index based on disk from Microsoft, I think, which is more like for disk, right? But I agree with you; for this scale, it's not convenient. But also, I think it's not only about vectors. This argument that we need to save on storage costs is insane to pay for storage and memory as well when we have replicas. In Postgres, if it's a physical replica, it's everything. You replicate everything, all indexes, everything. You cannot replicate; even there's no ability to replicate only one logical database; you need to get all logical data, all clusters. That means that you multiply costs for storage and for memory. It would be so great to have some cheap storage, maybe with partitioning, as much as automated as possible, and offload all data to S3 as basically you consider. S3 is great for this. I remember I explored turbopuffer through Cursor. It's great to see the documentation. I knew Cursor is using PostgreSQL. It was a few months ago. But then I heard they considered moving to PlanetScale. That was before PlanetScale announced support of PostgreSQL. So I was thinking, are they switching to MySQL? And then I saw vectors are stored in turbopuffer. Great. Then I learned that several of our clients who get consulting from us and use PostgreSQL also store vectors in turbopuffer. It was a surprise for me, and I started to think, oh, that's interesting. And then I checked your talks. I think you also mentioned there that if we... So there is this approach with the SPFresh algorithm, right? It's not HNSW—different types of index. But also some additional interesting ideas about economics you mentioned, right? Can you elaborate a little bit?

**Simon Eskildsen** [16:40]:

I think it might be helpful to just talk at a very high level about the different algorithms to do vector indexing. I'll try to simplify it as much as possible. Oh, and we could dig into it further if you want. Fundamentally, the simplest way to do vector search is that you just store all of the vectors in a flat array on disk, right? And then on every search, you just compare the query vector to all of those. The problem with that is that you very quickly run up against bandwidth limits, right? If you have a gigabyte of vectors and you're searching that at maybe ten gigabytes per second, if you can exhaust the memory bandwidth, which is unlikely in a big production system, you're only doing maybe five queries per second, and their query latency in the hundreds of milliseconds. So if you have very few queries and you don't care about latency, this can be feasible on a small scale. Lots of people are doing that in production. But if you want to search a million vectors in less than a couple hundred milliseconds, and maybe ten milliseconds, and that's part of a bigger pipeline, you need some kind of index. The problem with indexing vectors is that there is no known way to do it perfectly. If I search for a query vector about fruit or whatever, I know that if I'm searching for banana, I get the closest fruit. Maybe, I don't know, maybe that's a plantain, I don't know. Right in the cluster. But you have to build an approximate index in order to make this faster.

**Michael Christofides** [17:57]:

Because of too many dimensions. For a small number of dimensions, there are classical tree-based approaches that have been around for years, but for high dimensionality—hundreds of dimensions and up—yeah.

**Simon Eskildsen** [18:06]:

That's right. Yeah, there's KD trees and so on for the smaller dimensional space, which we can use for geo-coordinates and simpler geometry. For very high dimensional spaces, these things fall apart. The curse of dimensionality, it's called. So they're very large; it's also important about the vectors. If you have a kilobyte of text, it can easily turn into tens and tens of kilobytes of vectors, which is why separating it into cheaper storage makes a lot of sense. So there are two fundamental ways that you can index the data. There's the graph-based approaches, HNSW and DiskANN, which were the two you mentioned before. And there's the clustering-based approach. The graph-based approach is phenomenal if you can store all of the data in memory and you have very high QPS and very low latency requirements. So if you have, let's say, a hundred million vectors and you're searching that at, you know, a hundred thousand queries per second and you need very low latency, you're not going to beat HNSW; it's going to create a very good graph to navigate it with. The problem is that it's very expensive. And the other problem is that almost no workloads in the real world actually look like this. HNSW got very popular because it's fairly simple to implement correctly and it's very simple to maintain. When you create the graph, which is essentially just points that are close in vector space are close in the graph, it's very simple to incrementally maintain. You put one thing in, you search the graph, and then you add it; there are very simple rules. You can implement something like HNSW in a few lines of code if you did a really very simple implementation of it. The problem with HNSW is that every time you do a write, you have to update a lot of data, right? In database land, we call this write amplification, where every byte or every page you update, you have to update a lot of others. The reason for that is that you add something; you add a graph; you add a node in the graph, and then you have to update all the other things that do connections to that node in the graph. This works great in memory because memory is very fast at updating and very fast at random writes. But the problem is also on the read path. Memory is very fast at doing random reads. You can do a random read at a hundred nanoseconds. But a random read on S3 or on a disk is much slower, into hundreds of microseconds to the hundreds of milliseconds on S3. And in a graph, right, you don't really—there's no speculation that helps, right? If you start at the middle of the graph and then greedily navigate the graph from the query vector to the closest matching vectors, well, every single time you do that, there's a round trip. On S3, that's a round trip that takes hundreds of milliseconds, so you're sort of navigating from the root; it's like two hundred milliseconds, you go out one, two hundred milliseconds, you go out another one, two hundred milliseconds. In general, for HNSW on a million, this might be in the tens of round trips. That just gets very slow, right? This is in the seconds to do this on S3, and even on a disk, this very quickly adds up to tens of milliseconds. That's the fundamental problem with graphs. Now DiskANN is essentially using a lot of the same ideas as other graph-based indexes, but instead of trying to have the tens of round trips that HNSW has, that's very good for memory, DiskANN basically tries to shrink the graph so there are fewer jumps, right? Instead of 200 milliseconds thirty times, it tries to get it to maybe six or seven times or ten times by shrinking the graph as much as possible. That's essentially the insight in DiskANN. The problem with DiskANN is that after you have added more than ten or so ten to twenty percent of the size of the data, you have to rebuild the entire graph, which is an incredibly expensive operation. That is absolutely terrifying to me that someone has been on call for large databases to just have—you could max out like, you know, 128 cores rebuilding this graph in production, and it could take you down to 3 a.m. because you don't know when you hit that threshold, and if you don't do it, then the approximations start getting bad, and you start getting bad search results. The nice thing about the graphs is that they have very low latency, but they're just very expensive to maintain. Now the first way, then there's the other type of index, which are the clustered indexes. Clustered indexes are very simple to picture in your head, right? If you have a cluster of, let's say, you took out every song in Spotify and you cluster them in a coordinate system, and you can visualize this in two dimensions. If you then plotted all the songs and the songs that are adjacent are, of course, also adjacent in the underlying vector space, genres will emerge, right? There'll be a rap cluster, there'll be a rock cluster; you zoom in and you get, you know, like little sub-clusters. I don't know, death metal, black metal; I don't know what all the different rock genres are, right? Somewhere in these clusters. You could generate great recommendations based on that because you can look at, okay, what did Michael listen to, and what are some songs that are close by that he hasn't listened to, and same for Nikolay. It's very simple; you create a clustering algorithm that basically just tries to divide the data set into clusters. When you do a query, instead of looking at all the vectors, you look at, well, clearly the user is asking about a rock song, so we're only gonna look in the rock cluster. That way, you divide down the number of vectors that you have to seek. Now the problem with this is that if you have everything in memory, it's not necessarily as optimal because you might have to look at more data than you do in a graph-based approach. Because RAM has such good random read latency, the penalty is not necessarily worth it if everything is in memory at all times. But this is great for disks, and it's great for S3 because I can go to S3 and in two hundred milliseconds get gigabytes of data back, right? It doesn't matter if I'm getting like, you know, a megabyte or a gigabyte; I can often get that in the same round-trip time if I exhaust the network. So if you don't go into S3, basically you have to download all the clusters. So let's say the clusters are JSON blobs—to really simplify this—and then you just look at the closest clusters to your query vector, and then you download, you know, cluster one dot JSON, cluster two dot JSON, whichever ones are close, in two round trips. Now instead of on the graph-based ones where you're doing two hundred milliseconds, two hundred milliseconds, two hundred milliseconds to navigate the graph, you just have to get the clusters, and in two hundred milliseconds, you get all the clusters that were adjacent. The nice thing about these clustered indexes is that with algorithms like SPFresh and lots of modifications to them, we can incrementally maintain these clusters. You can imagine that when you add a vector, you just have to add it to the cluster, and it's just one write. The write amplification is very low. Once in a while, that cluster will grow beyond the size—let's say it's a thousand elements long—and you have to split the cluster, and then you have to do some modifications. That's essentially what SPFresh is. There's a little bit higher of write amplification, but it's stable in the way that you never reach this threshold where, okay, I've added 20% of the data set; I have to rebuild the entire thing as you do in DiskANN. HNSW doesn't have to do that, which is why it's very nice, but it's just slow still. SPFresh, we think, writes a really, really nice set of trade-offs where it's going to be a little bit slower, but slower in terms of, okay, instead of a search returning in a millisecond, it might take five milliseconds, and just no one cares in production for search. This matters for a point look-up into a relational database, but for search, it's a perfect set of trade-offs.

**Michael Christofides** [24:52]:

Mm-hmm.

**Nikolay Samokhvalov** [24:53]:

Question on the cluster splitting. Does that mean we don't ever need to rebuild the whole index? Because I think that was a limitation of the first cluster-based—IVFFlat, I think we started with in pgvector, and that didn't have the splitting as far as I'm aware. Therefore, we had to rebuild the whole index every time if the data changed significantly.

**Michael Christofides** [25:15]:

That's right. I mean, there's also merge, right? There was also not only split; there is also merge in the SPFresh as I remember.

**Simon Eskildsen** [25:21]:

Yes, there's merges as well.

**Michael Christofides** [25:22]:

Makes sense.

**Simon Eskildsen** [25:23]:

And because you might do deletes, which are also a pain. To my knowledge, pgvector does not implement SPFresh; it is very difficult to implement correctly. But...

**Michael Christofides** [25:37]:

Also funny, I did a little bit of research in May, I think, when I discovered turbopuffer. I started reading about this. I saw the original implementation was actually on forks of PostgreSQL. Digging into SPFresh, I saw some—you have to dig pretty deep—like in some Microsoft repositories also, I think some Chinese engineers were involved, something like that. I saw some repository which was basically forked PostgreSQL, and the initial SPFresh implementation was on top of that. Maybe I'm hugely mistaken, but I saw something like this. But it's hard, I agree. And oh, I also recalled what I wanted to ask. I was lost in my previous question because it was too many things. I recalled in your talks, you discussed that S3 is ready to store data. S3 is ready because over the last few years, they added important features. Can you recall what we talked about in the talks?

**Simon Eskildsen** [26:34]:

Yeah, I mentioned that there were a few prerequisites to building a new database like this. There's a reason why a database like turbopuffer hasn't already been built. It's a new storage architecture that's only really possible now. The three things that have enabled a database like turbopuffer to exist with this sort of pufferfish architecture, right? When the pufferfish is deflated, it's in S3. When it's somewhere in between, it's in SSD, and then it's in memory when it's all the way inflated. The reason that's possible is because of three things. The first one is our NVMe SSDs. NVMe SSDs have a new set of trade-offs. They act completely differently than other disks, right? SSDs are just sort of like—the old SSDs were very fast, but NVMe SSDs have just phenomenal performance potential, where basically on an NVMe SSD, the cost per gigabyte is a hundred times lower than memory. But the performance, if you use NVMe SSD correctly, is really that you have to put a lot of concurrency on the disk. But again, similar to S3, every single round trip takes into hundreds of microseconds, but you can drive a lot of bandwidth. Old storage engines have not been designed for that. You have to design from the day that you write the first line of code; otherwise, it takes a very long time to retrofit. It happens to be that that exact usage pattern is also what's very good for S3. NVMe SSDs were not available in the cloud until 2017, 2018. So this is, in database language, relatively new. The second thing that needs to happen is that S3 was not consistent until December of 2020. I think this is the most counterintuitive because most of us just think that it always has been, but it hasn't. What that means is that when you put an object on S3 and then you read it immediately after, you were—after December 2020—guaranteed read-after-write consistency. The third thing, and this is very informed by the fact that I was on call for Shopify for so long, when you're on call for a long time, you gravitate towards very simple systems and ideally systems that are constantly tested on their resiliency so you don't get paged when something abnormal happens.

**Michael Christofides** [28:45]:

Yeah.

**Simon Eskildsen** [28:46]:

For us, what was very important for us to be on call for a database for Justine and I was that it only had one dependency. That dependency could only be one of the most reliable systems on earth, which is S3, Google Cloud Storage, right? And the other derivatives like Azure Blob Store and so on. They're very, very, very reliable systems. But you could not build the metadata layer on top, right? So Snowflake and Databricks and others that are built on top of this in the last generation of new databases needed another metadata layer, some consensus layer like FoundationDB or their own Paxos or Raft protocol to essentially enforce the read-after-write consistency, but also to do various metadata operations atomically. Later, S3 added compare-and-swap (conditional writes) for objects. What compare-and-swap allows you to do is to put a file, let's say metadata.json on—you download the file, you do some modifications to it, and then you upload it again with a version number, and you only upload it if the version number is the same, right? Basically guaranteeing that you did an atomic operation and nothing has changed in the interim. Very important when you're building distributed systems, right? You can really implement anything on top of that as long as you're willing to take the performance hit of going back and forth to S3. Of course, they have a whole metadata, Paxos whatever thing to implement that. In GCS, it's Spanner, but I don't have to worry about that, right? That's for them to formally verify and whatever they need to do to uphold those constraints. Those were the three things that needed to happen, right? That is requirement number one to build a new database. That's what was in the air for turbopuffer to grab, right? The second thing that you need for a new database is that you need a new workload that's begging for that particular storage architecture, right? So for Snowflake and Databricks, we saw that in, well, we want big scale analytics, and it doesn't make sense also. It's also for the, you know, dollar per gigabyte that we have to pay in operational databases and adding indexes on everything. So there was a new OLAP workload, and there was a decent acceleration of the OLAP workload with all the successful web applications. The new storage architecture on top of S3, but they have to do some more work because these APIs that I just mentioned weren't available. A new workload now is connecting lots of data to AI, and this storage architecture is a very good fit for it.

**Michael Christofides** [30:59]:

Yeah, well, so that's great. Thank you for elaborating about these three changes. But AWS also shipped something new recently—they announced S3 Vectors, right? What do you think about this compared to your system?

**Simon Eskildsen** [31:15]:

Yeah, I think that S3 vectors is—if you are writing the vectors once and you don't query them that much and you don't care that much about the query latency, it can be a useful product, in the same way that you might be using S3 today. But S3 vectors doesn't do full-text search, right? It doesn't do lots of these features that you need for a serious system. Even S3 vectors recommends that you load into OpenSearch. For archival of vectors, this can make sense. So there's still an operational niche for it, but there are lots of limitations that would make it very difficult for it to go into production systems, right? If you do a query to S3 vectors, it takes hundreds and hundreds of milliseconds to get the result back. Whereas with turbopuffer, you can get the result back in less than ten milliseconds.

**Michael Christofides** [31:54]:

Thanks to cache, right?

**Simon Eskildsen** [31:58]:

Thanks to cache, yeah.

**Michael Christofides** [31:59]:

Yeah, that totally makes sense. I still have skeptical questions, more of them, if you don't mind.

**Simon Eskildsen** [32:05]:

Let's go.

**Michael Christofides** [32:06]:

Yeah, one of them is your cache layer is on local NVMe SSDs, right?

**Simon Eskildsen** [32:12]:

Yep.

**Michael Christofides** [32:12]:

But why? Like, we could store it there. PlanetScale recently came to the PostgreSQL ecosystem, and they said, "Let's stop having fears about using local NVMe SSDs and ephemeral storage and so on." Like, it's reliable for the lower... And so on, like, let's do it super fast. Yes, I agree it's super fast, and four terabytes for a billion or three terabytes for a billion vectors probably won't cost too much because this price is usually in AWS, for example, in the instances which have local NVMe storage; it's basically included. The limit is many dozens of terabytes of local NVMe storage these days on larger instances. So we are fine to store not only vectors but everything else. So my question is, like, back from S3 to MySQL, for example. MySQL supports storage engines for many years. Have you considered building a storage engine for MySQL, for example, and using local memory?

**Simon Eskildsen** [33:19]:

You can't outrun the economics, right? The economics are still the same. You have to replicate, whether it's local or not, which is not necessarily cheaper, maybe only marginally than a network volume. You still have to replicate it three times, right? You still have to put it on three machines; you still need all the cores and memory and so on that you would need on the primary to keep up, unless you start to have a heterogeneous replica stack, which for a variety of reasons would be a really bad idea. So you're still paying for all of that. Now, up to a certain point, that makes a lot of sense, right? If I have a customer who gets on a call with our sales team and they have a couple million vectors in pgvector, there's no reason to move off of it. That is perfect; you should not be taking on the complexity of ETLs and so on. But if you have like tens of terabytes of vector data, it is not economical for a lot of businesses. Now, for some businesses, it is, right? But the art of a business is to earn a return on the underlying costs. For some businesses, it's very, very challenging to earn a return on storing this on three replicas of this vector data. It's generally not valuable enough to the business. So turbopuffer doesn't make sense to as a storage engine into MySQL or into PostgreSQL. It's just fundamentally not compatible with the way that we do things outside of replica chains, right? You could maybe come up with a storage engine where you page everything into S3 and all of that, but you're now trying to build a new database inside of an extremely old database, right? The storage layer is, especially more so in PostgreSQL than in MySQL, is very, very weaved into how the query planner works and all of that. So at that point, you're rebuilding the query planner to be very round-trip sensitive. You're rebuilding your storage layer. It doesn't make sense anymore; it's a completely different database.

**Michael Christofides** [35:07]:

Okay, another skeptical question. I go to the turbopuffer website, and by the way, great design, very popular these days with monospaced font and those types of graphics. They advertise for one billion scale, one billion vectors for one billion documents. But if I check one billion, the concept of namespaces pops up. If I choose one billion, if I use a hundred million, it's okay; I can't have one namespace. But if it's one billion, there is a warning that probably you should split it into ten namespaces. And this means ten different indexes, right?

**Simon Eskildsen** [35:51]:

That's right.

**Michael Christofides** [35:52]:

Yeah. So it's not actual one billion scale. Or like something is off here in my mind. One billion is a single index for the index, a single index. If we talk about one billion but divided by ten and collections and indexes also, like, it's already a different story. Can you explain this to me? I saw this in the beginning, and I'm happy in this position I can ask the founder himself about this.

**Simon Eskildsen** [36:27]:

Yeah, we try to be extremely transparent in our limits, right? I think your mental model is correct. Before this, we just had a limit that said that we could do in a single namespace around 250 million vectors, right? But even that's if—even that is a simplification because how big are the vectors? If they're 128 dimensions, they're a lot less space, which is ultimately what matters here. Everybody's probably also put the gigabytes. So when we in the past had just 250 million vectors on there as a limit, people came to us and said—or we knew that people weren't testing with us because they wanted to sort of search a billion at once. They didn't realize that you could just do ID modulus N and then you could do a billion, and it would be very economical for them. So we sort of had to, you know, put in the docs like, yes, you can do a billion at once, but you have to shard it. Now, I would love to handle that sharding for you, right? I mean, that's what Elasticsearch does, and it's what a lot of databases do because the only way to scale any database is sharding, right? You don't get around it. The question is where the complexity lives, right? Does the complexity live inside of the database to handle it for you? Or some of the most sophisticated sharding you will find lives inside of Cockroach and Spanner and these kinds of systems. The simplest type of sharding is what we're exposing, where every single shard is just a directory on S3, and you can put as many of them as you want, and you can query as many of them as you want. Over time, of course, we need to create an orchestration layer on top of that so that a logical namespace to the user is actually multiple namespaces underneath. But we're challenging ourselves to make every individual namespace as large as it possibly can. When I ran an Elasticsearch cluster or was involved in scaling Elasticsearch clusters, every shard was around 50 gigabytes. That was roughly what was recommended.

**Michael Christofides** [38:14]:

Quite small, right?

**Simon Eskildsen** [38:15]:

It's quite small, right? Like on PostgreSQL, it's a lot larger than that; it's basically the size of the machine. But the problem with a small shard size is that for something like a B-tree, right, it's obviously like it's log N to the number of searches you have to do. If you have log N and N is very high, well, that's a great number of operations that you have to do. But for every shard, there's sort of like an M log M, and now M is very high if the shard size is small and N is small. So you're doing a lot more operations. We want the shards to be as large as possible because, of course, you can get to a billion by just doing, you know, a thousand one million shards, but that's an incredibly computationally ineffective way to do it. So we have shards now for some users that we're testing that do almost a billion documents at once, right? But it requires some careful tuning on our end. We want to push that number as high as possible. The other thing with namespaces is that turbopuffer is a multi-tenant system, and we have to bin pack these namespaces. So if a namespace is five terabytes large, it's much harder for us to bin pack on node sizes that make sense. We have to strike a balance: we want shards as large as possible for ANN efficiency, but a single logical namespace that's many terabytes is much harder to bin-pack onto nodes. We have to index without slowing the indexing down. Because the larger an ANN index gets, the harder it is to maintain the index. Those are the constraints we work within. Over time, we update these limits continuously. You will see that shard size increase. But you're not gonna find anyone who's doing a hundred billion vectors in a single index on a single machine, a.k.a. M is one and N is a hundred billion. But we want to get as high as possible because that's where we're the most price-competitive. That's how we get our users the best prices, and we see the most ambitious use cases.

**Michael Christofides** [39:57]:

And the shards are these actual shards or partitions? So it's like shards meaning that different compute nodes behind the scenes are used. If it's ten namespaces, is it ten shards, ten different compute nodes?

**Simon Eskildsen** [40:11]:

An index space for us—and this is also why we've chosen a different name for it, right? An index space to us is a directory on S3, and which compute node that goes to is essentially just a consistent hash of the name of the prefix and the user ID, right? So compute node one could hash to maybe, you know, a hundred thousand different index spaces, and they share the compute, and then we scale the compute without—and that's why the bin packing problem comes in.

**Michael Christofides** [40:39]:

It's flexible.

**Simon Eskildsen** [40:39]:

Yes, and that's also part of how we can get really good economics.

**Michael Christofides** [40:43]:

Yeah, I wanted to shout out again; the front page is beautiful. It explains the basics, architecture, and pricing right here; latencies right here in case studies. This is like how it should be for engineers, you know, so you quickly see all the numbers and understand. That's great.

**Simon Eskildsen** [41:09]:

I think it just comes from the fact that I was on your side of the table my whole career.

**Michael Christofides** [41:14]:

Right.

**Simon Eskildsen** [41:15]:

I've been buying databases, evaluating databases, and sometimes I swear I end up on a database website, and I don't know if they're marketing a sneaker or a database.

**Michael Christofides** [41:24]:

Yeah, go figure out storage, vCPU, price, IOPS—it's always a task.

**Simon Eskildsen** [41:32]:

We really wanted to just put our best foot forward: the diagram is right there. Okay, this is my mental model; this is what it does; this is what it costs; this is how fast it is; these are the limits; these are the customers and how they use it. You go to the documentation; we talk about guarantees; we talk about trade-offs, the kinds of things that I've always looked for immediately to slot it into my model. Because I don't want you to use our database at all costs; I want you to use it if it makes sense for you. I want it to be a competitive advantage to you. I want to save you millions of dollars a year. In order for that to make sense, you have to just put all the bullshit aside and make it very clear what you're good at and what you're not good at.

**Michael Christofides** [42:08]:

What about, for example, if I know there is vector search and full-text search supported, right? But what about additional filtering when we have some dimensions we want to filter on them? Is it possible? Like, for example, some categories or time ranges or something? So possible, right? In both types of search and database.

**Simon Eskildsen** [42:31]:

I'll go back to my line before of every successful database eventually supports every query, right? And we're, yeah, I don't know, I like maybe five percent of the way there. So we support filters. Filtering on vector search is actually very difficult, right? Because if you do something like, let's say I'm searching for banana, and it's an e-commerce site, and I have to filter for ships to Canada. Well, that might cut off a fruit cluster, which is actually where I want it to be. Then, like, you know, I get to the banana on a T-shirt cluster, but it's really far away. You have to do very sophisticated filtering to get good results. I don't know what pgvector does, but I know that our query planner is recall-aware. It does a lot of statistics and distribution of where the vectors are to ensure that we have high recall. For a percentage of queries in production, we check against an exhaustive search what the recall is, the accuracy of the index. I don't know if pgvector does this, but I know it's been a monumental effort for us to do it.

**Michael Christofides** [43:25]:

Headache there. It's a big headache. It's still a good point that additional filtering can change the semantics of the search. If it's searching for bananas where some attribute equals `nano`, it's different.

**Michael Christofides** [43:37]:

That's one thing they've implemented relatively recently is iterative index scans. They had a problem where, for example, you'd put a limit; you'd do a similarity search with a limit, and you'd ask for a hundred results, and you'd get back fewer, even though there were. They had a problem, so they go back to the index and request more. It's like a workaround, I'd say, more than a solution, but it is pretty effective for a lot of use cases.

**Simon Eskildsen** [44:04]:

I think the problem there is that I would be very skeptical of the recall in a case like that, right? Because, okay, so you just like you got one hundred, but you probably should have looked at a lot more data to know what the true top K was. So...

**Michael Christofides** [44:17]:

Yeah.

**Simon Eskildsen** [44:17]:

You know, we—that solution was not acceptable to us. We had to go much, much further to ensure high recall. I think the problem with a lot of these solutions is that people are not evaluating the recall because it's actually very difficult to do. It's very computationally expensive. You're not gonna have your production PostgreSQL run an exhaustive search on millions of vectors. It's too dangerous to do that, or you're doing your transactional workloads. But again, if you're hitting these kinds of problems, then it may be time to consider maybe for a subset of your workloads whether something more specialized makes sense, whether the trade-offs make sense to you. But I think with the pre-filtering and post-filtering, it can be very challenging to create a query planner that can do that well. So we support all the queries, right? You can do range queries; you can do ad hoc queries; you can operate with arrays; you can do all types of queries, set intersections that people use for permissions. We can do full-text search queries; we can do GROUP BY; we can do simple aggregates. We can do more and more queries, and we're constantly expanding that with what our users demand from the system that we built.

**Michael Christofides** [45:17]:

One more. Another one question. I know it's not open source; there is no free version at all even. I'm better sure it was a decision not to go with open core or open source model at all. Can you explain why?

**Simon Eskildsen** [45:29]:

Yeah, I think there's never been any particular resistance to open source. I mean, I love open source. The reason we're not open source is because open source is—if you want to do it well, it's a lot of effort. It's also a lot of effort to build a company. It's a lot of effort to find product-market fit. We decided to pour all of our energy into that. It's similar to the argument for the minimum. The only thing that a startup has over the big incumbents is focus. Our focus has been on the customers that are willing to pay. One day, I would love to give a free tier. I would love for people's blogs to run on turbopuffer. But for now, I'm afraid that we have to prioritize the customers that are willing to put some money behind it. It's not because of hardware economics on our side or anything like that; it's really just that we need to know that you're willing to put some money behind us so we can give you amazing support, right? A lot of the time, there'll be engineers working on the database who are supporting your tickets, and we can't do that with a free tier.

**Michael Christofides** [46:30]:

Mm-hmm, mm-hmm. Yeah, makes sense. Well, yeah, like we, Michael and I have different points of view on this area.

**Michael Christofides** [46:37]:

I think that was a good answer. I'm on your side, Simon. I had a question on the full-text search and semantic search. We had a discussion a while back about hybrid search. Are you seeing much of that where people are wanting to kind of...

**Simon Eskildsen** [46:56]:

Yeah, we do. What we see is that the embedding models are pretty phenomenal at finding good results for subjects that they know about, right? And terms that they know about. You know, you run a project called Pg Mustard, right?

**Michael Christofides** [47:12]:

Yep.

**Simon Eskildsen** [47:12]:

Maybe the embedding model doesn't know. I think it's popular enough that it will know what it is, but let's say it didn't know. Then it puts it in the cluster close to ketchup, and the results...

**Michael Christofides** [47:22]:

Yeah.

**Simon Eskildsen** [47:22]:

...were just horrible. The full-text search tends to be really good at this, like recall for things that the embedding model can't know about. If you're searching for a SKU, you know, these TV SKUs, it's like, you know, it's indecipherable. Actually, the embedding models are actually quite good at these because they happen enough on the web. But imagine that it wasn't. That's where full-text shines. The other thing is prefix-style queries. So if I'm searching for "Si," the embedding model might latch onto something in Spanish instead of the document that actually starts with "Simon." Sometimes you just need to complement these two, and I think that's why we've doubled down on the full-text search implementation in turbopuffer to do this hybrid. People can get a lot of mileage out of embedding models alone.

**Michael Christofides** [48:07]:

Wonderful.

**Nikolay Samokhvalov** [48:08]:

Yeah, thank you so much. It was super interesting. Good luck with your system. I think it's an interesting idea, I guess, for PostgreSQL users who need, as we said, to store more and have all characteristics to both have. Maybe it's worth considering, right, to keep all OLTP data, all the TP workloads and data in regular PostgreSQL while moving vectors to turbopuffer, right?

**Simon Eskildsen** [48:38]:

I mean, this is similar to, you know, people have taken workloads out of PostgreSQL. Updating a posting list is a very expensive operation in a transactional store. It's similar in an ANN index; updating that with the same kinds of semantics that PostgreSQL upholds is very expensive. I've ripped full-text search out of PostgreSQL many times because it's very, very expensive to do. We do that because we don't want to shard PostgreSQL because it's a lot of work. So we start by moving some of these workloads out. What's out first? Search is one of the early ones to go.

**Nikolay Samokhvalov** [49:09]:

Mm-hmm. So your point is that it probably postpones the moment when you need to shard.

**Simon Eskildsen** [49:15]:

That's right.

**Nikolay Samokhvalov** [49:16]:

Interesting.

**Simon Eskildsen** [49:16]:

Same reason from Memcache and Redis, right? You separate out these workloads as soon as possible to avoid sharding.

**Nikolay Samokhvalov** [49:24]:

Mm-hmm.

**Simon Eskildsen** [49:24]:

Kick the can.

**Nikolay Samokhvalov** [49:25]:

Yeah, interesting idea. Interesting idea. Okay, again, thank you so much for coming. It was also an interesting discussion; I enjoyed it a lot.

**Simon Eskildsen** [49:32]:

Thank you so much.

**Michael Christofides** [49:33]:

Yeah, very nice to meet you. Thanks for joining us. Take care.
