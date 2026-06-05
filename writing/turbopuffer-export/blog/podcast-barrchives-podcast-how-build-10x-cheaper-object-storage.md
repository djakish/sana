# How to build 10x cheaper with object storage

August 05, 2025•Barrchives Podcast

[Video 3](https://www.youtube.com/watch?v=3KB_Ly6ZJ_E)

## Transcript

**Simon Eskildsen** [0:00]:

Trade-offs are very important in a database. That's the fundamental thing with databases and why some databases are good at things that other databases are not as good at.

**Barr Yaron** [0:07]:

Welcome back to Barrchives. I'm your host, Barr Yaron, and today I'm excited to have Simon Eskildsen, co-founder and CEO of turbopuffer, joining us. And turbopuffer powers search for fast-growing startups like Cursor, Notion, and Linear.

**Simon Eskildsen** [0:22]:

The fundamental trade-off for turbopuffer is that writes are slow, right? We have to commit them directly to object storage, so it takes hundreds of milliseconds. You're not going to do an OLTP workload on something like that; it just doesn't make sense. You can't do checkouts for e-commerce on something like that unless you get it all down in a single transaction. And then once in a while, you'll hit a node that doesn't have the data on disk, and you'll get a cold query, and it will be a couple hundred milliseconds instead of tens of milliseconds. For search, that's a perfectly acceptable trade-off. For high-frequency trading, maybe not so much. We happen to think that this set of trade-offs are pretty phenomenal for a lot of workloads, especially search.

**Barr Yaron** [1:06]:

Hey, Simon. I'm so excited to have you on today. It's sort of insane to me that you started turbopuffer in 2023, and it's become such a beloved product. You're running hundreds of billions of vectors. Top customers are building on top of turbopuffer, so I'm excited to get into it today.

**Simon Eskildsen** [1:23]:

Thank you so much for having me, Barr.

**Barr Yaron** [1:25]:

Okay, well, let's start with your aha realization around turbopuffer. You were a principal engineer working on infrastructure at Shopify, but then you had your period where you consulted with startups, helping them with their infrastructure and scalability issues. So when in this process of working with companies did you realize maybe there is a need for turbopuffer? It's time to build this thing? How did that happen?

**Simon Eskildsen** [1:48]:

Yeah, I think maybe the full background is useful here. So yeah, as you said, I spent almost a decade working on infrastructure at Shopify. It was kind of a ragtag team of software developers who learned the infrastructure as we went, along with the operations people, to just make sure that this Rails app continued to scale as the company did. When I joined, we were doing a couple hundred requests per second, and when I left in 2021, we had peaks of more than a million requests per second. The hardest thing to scale through all of that is the data layer. So as part of that, my co-founder Barr Yaron and I spent thousands, tens of thousands of hours—well, probably 10,000 hours at this point—scaling every single part of the data layer of Shopify: MySQL, Redis, Memcached, Elasticsearch, like all of these things and proxies in front of it. I have some experience running these machine-based solutions at scale, and they're really good for the kind of e-commerce search that we needed, where it's very important to search almost all of the data a lot of the time. But it always seemed to me that there might be a better way to do search. It didn't occur to me until I spent a couple of years bopping around, helping my friends' companies in small increments with their infrastructure challenges, what the future of search might look like. One of the companies, my friend's company Readwise, asked me if I could build a small recommendation engine after I was done spending a bunch of time tuning their Postgres auto vacuum, which is like the most common scaling challenge I think in the 2020s. We wanted to build a recommendation engine, and I thought that vectors just looked amazing because one of the search problems that we talked a lot about at Shopify was sort of mapping the vocabulary of the user with the vocabulary of the store. So you search for "red dress," and they have a "burgundy skirt," and you search for "shoe," and they have some lime green sneaker, and it just doesn't come up, right? Because you're searching for strings, not for things.

**Barr Yaron** [4:05]:

The words are not...

**Simon Eskildsen** [4:07]:

Exactly. You're searching for strings, not for things. And so you've got to turn strings into things. Vectors are really good at that, right? You chop the head off of the LLM, and out come these numbers that you can plot in a very large coordinate system, and things that are adjacent in that coordinate system are also adjacent in the real world. The LLMs were wonderful for that. So for my friends at Readwise, we made a small recommendation engine that did this, and it was pretty good. Without much tuning, it actually did an okay job recommending articles, and it makes sense because it's trained on articles on the web, so it was very good at it. But when I ran a napkin math on how much this was going to cost, it was close to maybe $30,000 to $40,000 on the reputable vector database at the time. It made sense; it was new, but it turned out that all of this was stored in memory. At Readwise, the founders put this in the bucket of, "Well, that seems really neat, but it just costs too much, so we're going to wait for the cost to go down as token costs have also come down." I couldn't stop thinking about it. I was like, "Why has no one built a database that takes advantage of the things that we have available to us now?" Because it seems like the perfect trade-off for search. We have NVMe SSDs; they're about 100 times cheaper than RAM, but the memory bandwidth is only maybe about 5 to 10 times lower. We have S3 that is finally consistent, which is a very nice property to have when you're building a database, which happened in late 2020. And then we also have compare and swap on object storage, which means that we can now build a database that, like a pufferfish, inflates into the memory hierarchies from object storage into NVMe and finally into memory, with the only downside being that all the writes have a higher write latency. We thought that this was a perfect set of trade-offs for search. So that is the long story of how I went from when it's building to recovery.

**Barr Yaron** [6:03]:

I mean, that's very helpful. And I like that, you know, for someone who spends a lot of time thinking about search, I'm surprised I haven't come across "things, not strings," but it's a good tagline. I mean, it makes sense. You have these new capabilities, like you mentioned, NVMe SSDs in the cloud, consistent S3, and compare and swap. And then you have what you had mentioned, which is sort of this new workload of LLMs. What are some of the other things that made this the right point in time, whether it was workload, data type, or other capabilities?

**Simon Eskildsen** [6:34]:

Yeah, so I think if you want to build a new database, you need two things. You need a new workload, and you need a new storage architecture. The new workloads seem to be that there's an enormous amount of data that wants to be connected to LLMs. The models are just hungry for more data, and they're hungry at reasoning with the data. But in order to do that at the scale that was required, we also needed to change the economics of the storage. If you take all of the data and store it in memory as vectors, then these vectors are 10, maybe 100 times larger than the dataset itself because if you have a kilobyte of text, that turns into usually tens of kilobytes of vectors. So the need for a new storage architecture is even more prominent there in terms of just the pure costs. If you store a gigabyte of data on a disk, it costs you about $0.20, so $0.10 for the gig, for $0.10 per gigabyte of disk, and then you run them at around 50% disk utilization. Then you replicate it three ways, so you end up paying about $0.60 all in per gigabyte of data that you store versus the $0.02 per gigabyte when you store it in object storage. In case you're accessing it a lot, you only have to replicate it to a single machine, which can cost you maybe somewhere around $0.05 per gigabyte. So all in, you're like an order of magnitude cheaper than the base cost of replicating this on disk. So the two things you need for a new database are a new workload, which is connecting lots of data to LLMs, and the second thing you need is a new storage architecture, the new storage architecture being that object stores are a source of truth, and we just cache the data aggressively that needs to be accessed a lot.

**Barr Yaron** [8:17]:

Let's actually walk through, okay, you had this realization when you were helping Readwise. You built, you eventually built turbopuffer. What goes into the simplest version of turbopuffer, the initial version that you put in front of a first customer? I know you've done a lot of optimizations and work since then. So architecturally, what goes into it, and what does it take to build that?

**Simon Eskildsen** [8:39]:

It takes a lot of embarrassment, I would say, to put out that version. I think there's a lot of these startup platitudes that you hear and that you don't fully internalize until you're in it. The play-by-play here was that it was March of 2023, and I had this idea, and I was talking through it with a friend. He really encouraged me to just go for it, and so I started thinking about it. I was a really good friend; it's actually the friend who since designed the website, which is now a big part of our identity.

**Barr Yaron** [9:10]:

Oh.

**Simon Eskildsen** [9:10]:

I sat down and started working, learning everything I could about all the different vector indexing algorithms and then reasoning through which ones would work on object storage and which one wouldn't. I spent a summer up by a cabin in Canada and just completely focused on this first version of the database. It ended up being the simplest thing that I could possibly ship that had acceptable performance.

**Barr Yaron** [9:43]:

How do you define acceptable performance?

**Simon Eskildsen** [9:45]:

Acceptable performance is like a hot query around 100 milliseconds seemed good enough to ship, and a cold query around one to two seconds seemed good enough to me to ship with the economics that we had. There was no reason to me why this couldn't be as fast as the fastest one out there that was in memory, just with better economics, so you could really get all that benefits out of the gate. The first version of turbopuffer was literally just a file that was called centroids that was on object storage. You download the file of the centroids, and then you search through them all, and then every cluster in the vector index were in other files that were then downloaded into the second round trip into the process. That was it. I could go into more details on what that exactly means, but it was very simple. It was just two round trips back and forth to object storage. At the time, there was not even an SSD cache; I just put a caching engine in front of it. I ran the entire thing in a TMUX session on a single node in prod. It was literally the simplest possible thing that I could come up with that I could ship after that summer of running an inordinate amount of experiments on figuring out how to make all of this fast because it's not quite as simple as I put it to do the indexing and everything in a way that has high recall.

**Barr Yaron** [11:12]:

What's the biggest challenge there in version one?

**Simon Eskildsen** [11:15]:

The biggest challenge was that it wasn't clear what indexing algorithm you wanted to use. So at a high level, when you're building a vector index, you sort of have three options. The first option is the simplest one. It's like when you have a query vector, you compare it to every single vector in the target dataset, and you return the top k closest ones.

**Barr Yaron** [11:36]:

And then you have to look through everything.

**Simon Eskildsen** [11:36]:

You have to look through everything, and it works for, you know, if you have about a gigabyte of vectors, you can read that at maybe 10 to 20 gigabytes per second if you max out the machine, so you can do maybe 10 requests per second if you exhaust the machine. Latency will be around 100 milliseconds; it sort of works, but as you get into larger and larger sizes and more queries per second, it sort of starts falling apart. The second option is to use a graph-based index, and this is a lot of the rage. All of the existing productionized implementations were using this algorithm called HNSW. HNSW is essentially you can sort of, with heuristics, construct a graph where vectors that are adjacent in vector space are also connected in the graph. The problem with this approach is that if you store the data on object storage, every time you navigate a node in the graph, you have to go to object storage. The p90 to object storage is maybe 200 to 300 milliseconds. So every time you navigate, you start at the center, 200 milliseconds; you go one out, 200 milliseconds, 200 milliseconds as you navigate. This is really fast in memory because you only need to do maybe nine to 10 reads to go get all the closest vectors, but it is extremely slow on object storage. Many, many queries, even on disk, are slow because disks are not good at a lot of reads. We're doing very low bandwidth per read. HNSW is phenomenal because you just insert vectors, and they just go into the graph, and it works great. It's the economics that are difficult. If you're storing a billion vectors in an HNSW graph, you kind of have to store the whole thing in memory or maybe some of it to disk. It gets very complicated very quickly, and the costs become astronomical—tens of thousands, maybe even hundreds of thousands of dollars to store a billion vectors, which you can do at a thousand dollars with turbopuffer. So these orders of magnitudes of improvements in storage really come out, but this poses a challenge because the reason why HNSW is so popular is because it has very high recall, very high accuracy against the exhaustive search, and it's very easy to maintain. So that's why it was so popular. The third approach is actually, I think, almost the most obvious one if you just sat down and drew a bunch of vectors in a coordinate system, which is that if you draw a coordinate system, you imagine it's in 2D, then naturally... Actually, clusters will occur. If we go back to the e-commerce example, you can imagine some of the vectors that talk about dresses and skirts are in one cluster, some that talk about shoes are in another cluster, and some that talk about pants are in the third cluster.

**Barr Yaron** [14:14]:

On the assignment, I would greatly separate the dresses and skirts. I don't agree with that example, but yes, I see. Directionally, I see what you're saying.

**Simon Eskildsen** [14:21]:

Well, the skirts and the dresses are like adjacent-ish, right? But still. So there's probably clothing items that I don't know the name of that you would know the name of that are right in between—a romper maybe—and but the shoe cluster we can say is a little bit further away. But either way, the idea that you do that then is that there might be three natural clusters here, and so you take the centroids of those clusters. The centroid space is just the average of all of the members; it's like an artificial vector. It doesn't make sense really to take the average of, you know, a romper and pants and dresses, but that forms a centroid. Now, instead of having, say, 100 vectors, you have three vectors, one for each cluster. When you do the search, you just look at what is the most adjacent centroid to my query vector, and then you download only all of the vectors that belong to that cluster with that centroid. This is the most old-school way of doing vector search. You run a big clustering algorithm over the entire thing, and you return the most adjacent clusters, and you search those clusters exhaustively, basically. It works fine. It's not as fast as HNSW unless you're very careful about how you construct the clusters. But constructing the optimum clusters is essentially an NP-complete problem; it takes an enormous amount of time. So there's a lot of heuristics that go into it, just like the graph. But it works really well for disk, and it works really well for object storage because whether you're downloading 100 MB or 1 MB from object storage, there's just not a big difference. On disk, there's also not a huge difference. Of course, there is a difference, but it's not the same kind of difference as doing a lot of random searches, right? You can do a lot of this in a round trip with just a small extra penalty. So it works really well for disk because you just get the centroids, and then you get the clusters that match. You go to object storage, and it's the same thing. For memory, you can get away with a lot of random reads into a graph in the time span that you can read all of that memory. For me, figuring out and really moving myself away from the status quo—that everything should be a graph, and that to make graphs work on disk you just shrink them so they do less graph search (disk kNN–style ideas)—took a long time, because there was nothing really. Everything seemed to be trending towards the direction of graphs.

**Barr Yaron** [16:42]:

Yep. Yep. And so the most difficult part was actually making that fundamental architectural decision and making sure that it's the decision point and not the implementation of it.

**Simon Eskildsen** [16:52]:

I think it was, yeah, getting high recall on that kind of solution with something that worked performantly on object storage. I had some false starts. I started by using a Cloudflare Worker and doing it there and had to move to servers, had to build a small storage engine. I tried a bunch of different ways of making the index be online updatable so you didn't have to retrain the whole index every time you did enough writes. That took some time, building a simple imitation of the WAL. There was just a lot of—I probably did three or four rewrites before I shipped the simplest thing over that summer.

**Barr Yaron** [17:27]:

And then from shipping the simplest thing into getting it into the hands of your first customer, what does that look like?

**Simon Eskildsen** [17:33]:

Yeah, so when I launched it, I was kind of exhausted from having worked the whole summer on it, and I launched it in the beginning of October in 2023. I got a nice email from one of the Cursor co-founders. This is back when Cursor was a smaller team. Knowing that team so well now, I can imagine that they sat around the dinner table and said, "Oh, like these vectors are so large, and the query profile that we need for doing retrieval over a code base just matches so well that we can hydrate it into a cache when we actually query it. Only a percentage are active." They would have just come up with this. I don't know if they did or not, but either way, it slotted right into how they thought about how this problem should be solved with the right set of trade-offs for them. Graphs are great if you're searching like a billion products all the time and you're eBay or Shopify, but for something like Cursor, where so much of the data is inactive, this architecture made a lot of sense to them. So they reached out, and they just sent like 10 bullet points with a bunch of numbers—like what kind of cost they were running up against right now, why it didn't match their unit economics, what kind of load they had, what kind of features they would need. We just went back and forth a bit on bullet points. Cursor was growing really well in 2023, but it was not as big as it is now. I felt like I needed to go meet this team in person. I had the instinct that I just needed to fly to San Francisco but not make them feel bad about it. So I just said that I was going to be in San Francisco on Monday.

**Barr Yaron** [19:14]:

It's the classic move.

**Simon Eskildsen** [19:17]:

I didn't know at the time, but I went to their office, and we had some long discussions. I spent a bunch of time helping them also with their Postgres. I mean, they were growing a lot of the time, and they were a very, very small team. We spent a lot of time talking about their Postgres and how to tune auto vacuum, coming back to that. Then I told them how turbopuffer worked, where we were going with it, and we decided to partner. They moved all of their load over the coming weeks after that to turbopuffer back in 2023. By moving them to this new storage architecture with this new set of trade-offs, they were able to reduce their storage costs or their vector costs by 20x or 95%, which just matched their user economics a lot better.

**Barr Yaron** [20:08]:

Cursor, first of all, is a phenomenal first anchor customer, and they've grown tremendously. Also, their use case makes a lot of sense, right? Historically, customers have large vector indices with very high usage; only a fraction for Cursor need to be queryable at any point in time. They only need the index in memory for the period the user is actively querying the code base. It makes a lot of sense. When you thought about initial early customers once you had it in the hands of the first one, how did you think about the trade-offs of kind of like who turbopuffer is not the best fit for, where turbopuffer particularly excels, and how do you think that—or do you think that—changes over time? Because to your point on Readwise, some of it is we cannot build a feature because it's too expensive right now. Cursor saved a lot of money; they can do more. That's going to be true for a long tail of customers. So maybe your belief is just the thought market grows. How do you think about dividing the market and where turbopuffer slots? This is the short version of that question.

**Simon Eskildsen** [21:11]:

Yeah, I think that I didn't really think about any of those things at the time is the honest answer. I think that I can talk now about ideal customer profiles; I can talk about—I can use all these terms that I didn't even know at the time. But at the time, it just came from a strong instinct that we could make this 100 times cheaper. It is offensive to me that all of these existing incumbents are in memory because it feels like there's a lot of workloads out there, like the one I saw at Readwise, that really just cannot afford this and are okay with a different set of trade-offs than the incumbents at the time. I happen to think they were a really good set of trade-offs. I didn't know what the customers were going to look like. I was only thinking about Readwise at the time and thinking that there must be others out there and that it must be a common problem. Now I can talk in much more sophisticated terms. I was just sitting down a bit earlier today thinking about what kind of questions you might ask today, and one of the things that I reflected a bit on is just that the language—and I mean, you've also gotten to know me over the past few years—the language that you use to describe these things sounds like, "Okay, yeah, sat down, did the napkin math, built the database, got customers with the ICP," and it just looks like this master plan being executed. But it never looks like that from the point of view of the founder, and I think that any founder telling you that would be disingenuous. At the time, it just came from being immersed and having spent so much time in the napkin math soup and knowing exactly what things cost in the cloud down to the cent on almost every SKU, and then just thinking, "Hey, if we put these things together, we could build something very different, very different economics." There's got to be a bit of a Jevons paradox, you know, gas gets cheaper, people drive more thing at play here, and it turns out that that was right.

**Barr Yaron** [23:39]:

So Simon, I'll ask you something. I'll ask you it slightly differently and pointed, although I do want to get into some of the technical trade-offs, which is at what point in time did you gain conviction? Because you're like, "I'm doing this. I see that Readwise has this problem. I suspect this is going to be a problem for other people. It's a perfect use case for Cursor." But, you know, there have been many vector databases today in the past, and then there's also a subset of folks who are using things like pgvector on top of their databases. So at which point in time did you gain conviction that there is a large market here and this is what you want to do for many years to come?

**Simon Eskildsen** [23:39]:

I think that in the beginning, we were very set on scaling for Cursor and giving them an amazing experience. We picked up some other customers that believed in us very early. These customers that are your first signups and that join the Slack channels, it's a very special relationship even now, years later, that you have with them. At some point, one of our peers launched an architecture that looked very similar. At that time, we were just continuing to see people who really liked the product and they liked the performance. I think in early 2024 is when we started seeing just getting very serious conviction on the kinds of workloads. I would say that there was a day where one of our early customers, we showed them a quote. Previous to that, they were using another vector database with a different set of trade-offs that turned out to not be ideal for them, so they were paying for performance they didn't need. When I showed them the quote, they asked me to show them a quote for 10x the data volume because now they realized that this would unlock some product that they've wanted to build, but that the per-user economics previously were just holding them back. This was in around May of 2024, and that's when my conviction really dialed up. Now I think now that we're seeing how much the modern agents and models are spending just querying datasets has increased my conviction to just an inordinate level.

**Barr Yaron** [25:35]:

I mean, that's awesome. Let's talk a little bit about what you've learned with these customers. So we talked about what it took to make that first simplest version of turbopuffer. What are the core optimizations and changes that you've made since then? And then to the last thing you made, we'll get there later. I'm curious how agents play into all of this and what you think ideal storage for agents looks like. So we'll do the optimization so far and then the optimizations you see in the future.

**Simon Eskildsen** [26:00]:

Yeah, so turbopuffer V1, the team internally makes a lot of fun of it. They call it founder code. I call it the reason you have a job. The other day, someone was tagging a bot, a Cursor agent inside of our Slack, saying, "Hey, can you remove all the code done by Simon?" So there's a running joke to get rid of every single vestige of the first version. But it got us very far. I did not expect it to get us that far, but it was rebuilding the entire index periodically. It was very simple. We moved from a very simple binary encoding to zero copy. We moved away from Nginx very quickly; we moved away from running everything on one TMUX very quickly, and just maturing on that first engine. It became very clear in the beginning of 2024 that this initial engine was going to sort of reach end of life by mid that year, given the growth that we were seeing. Again, we knew we had a lot of room for optimization there, but at the time, another engineer joined us—a phenomenal engineer—and more or less, he was focused on just building a new engine based on the workloads that we've seen. Very write-heavy needed to do incremental maintenance of the clustered index. It took a lot of time to get that right, building on top of a proper LSM for object storage rather than the very hacky storage engine that I had written. So we sort of exhausted the potential of V1 by mid-2024, and then we completely replaced it with V2 in the fall of 2024. The V2 engine is like a textbook, very simple, sort of CS 101—at least initially was not so much anymore—implementation of an LSM on object storage with the trade-off that that comes with. Then it was using an incremental clustered algorithm called SPFresh to maintain these clusters without having to rebuild the world periodically. We switched over completely to that. There's a lot more optimizations we could go into now on the V3 engine, but we expect the V2 engine to be the foundation of what we iterate on for a very, very long time.

**Barr Yaron** [28:43]:

You know, you mentioned the indexing as sort of the big decision for V1. Between V1 and V2, what were the most challenging decisions? So, for example, you mentioned that one of your engineers focused a lot on writes, and there was a trade-off in terms of the number of writes. So, you know, what were the core decisions between V1 and V2 that you all spent a lot of time thinking about?

**Simon Eskildsen** [29:06]:

The biggest pain point really was to get to something that would maintain the clusters incrementally, right? Like taking, you know, suddenly, you know, you have one cluster, right? And then someone starts adding a lot of dresses and whatever into it, and you have to split the cluster to make the search efficient. This, when you're doing it at tens of thousands of writes per second over tens of millions of vectors, is a very difficult problem, and it's very important because otherwise index accuracy will degrade over time. It's not like a B-tree where it's very simple to prove that it just remains stable over time as you add and remove elements. It's very challenging to do. A paper came out around that time of incrementally maintaining these clusters, and I'd experimented with some of that during the first summer because it felt that there was an intuition that at some point you could split a cluster, and maybe if you took enough away from the cluster, you could merge it and things like that, but I could not get it right. There are a couple of good ideas in that paper.

**Barr Yaron** [30:06]:

Never talked about it, whenever that was.

**Simon Eskildsen** [30:10]:

Yeah, and I think we weren't even convinced that this paper was a good idea. Boyan, who implemented it, was certainly not convinced that it was even remotely possible to do this at a high recall. But we started working on it; we started experimenting; we saw good results. But we have had to do a lot of work to make this work properly at scale. I think that if datasets are not changing very much, you can get away with just rebuilding a world, and a lot of businesses will be able to do that. But if you want to maintain indexes with tens of millions, hundreds of millions of indexes, you really need to have something where you can maintain these without having to re-cluster the entire dataset, which is extremely expensive. So that was really the biggest development in the V2 engine was to move to this and then also redesigning the storage engine. The first storage engine was very simple in terms of like, "I'm going to put this file here, and it has this data," whereas the V2 storage engine is a key-value store, right? It's like an LSM where we think about compaction, and we think about SSTables and all of these different primitives rather than just a struct that is put into a file and zero copied out of that file. It is a much more structured thing to iterate on as turbopuffer supports more and more queries, also not just vector queries but also full-text queries and some of the aggregations we can do now and these kinds of things. So it was really a maturing of the database where V1 was get us to market, get some customers, and learn from the workloads because I think that it was clear to me that the workload that these AI companies were going to have was not going to be completely clear to us, and there was going to be a different set of trade-offs. We really learned on V1 what those trade-offs were that could go into the other engine, like very write-heavy, and we learned a lot about how long things should stay in cache for and so on and so forth.

**Barr Yaron** [32:01]:

Maybe to be explicit about that, I mean, write-heavy is one of the things, but if you had to summarize how AI-native workloads pressure databases in fundamentally different ways, how would you sort of in two sentences describe that?

**Simon Eskildsen** [32:13]:

Yeah, it's probably like a hundred to one, right? Read ratio might be something to aim for. For some, it's different, but that's something that we see. The other thing is compaction is fundamentally different on object storage than it is on a disk. There's no literature about that.

**Barr Yaron** [32:31]:

How frequently are you seeing writes and the number of writes, and how are you dealing with that?

**Simon Eskildsen** [32:36]:

I mean, the biggest thing about the number of writes is that turbopuffer is designed around doing everything to object storage and not having any metadata layer. I think writes, when you have to coordinate across multiple nodes, are very challenging to do, but we just commit files to object storage, and object storage is extremely scalable, so that's one way that we think a lot about writes. The other one was the incremental updating of the indexes, which is obviously extremely important if you're doing a lot of writes. Those are probably some of the things. I mean, when you think about compaction, you also want to know how many writes are coming in, how often do you have to compact the database, how do you compact it, how do you lay out the LSM. These things—the read-write ratio dictates all of those things. Not to say that turbopuffer is not phenomenal in the reads as well, but we do see a lot of writes.

**Barr Yaron** [33:32]:

The answer may just be it made no sense with the architecture, but was there ever a consideration to have a metadata layer?

**Simon Eskildsen** [33:38]:

There was. My co-founder and I spent a lot of time talking about whether we should have a metadata layer and felt like everything was leading us to that point. Richie, who you also know at WarpStream, was like early on, we kind of became friends because we were both building on the same architecture, and for them, it was a very clear decision, right? The Kafka protocol sort of required an enormous amount of coordination with the metadata layer. But we had the luxury of there not being a real standard for these search workloads, so we could design the protocol around not having a metadata layer if we could get around to it. But we really thought we would have to. We also thought that you would want to replicate just to make the writes faster and have lower latency. But it turned out that with compare and swap on object storage, we were able to do all the metadata on object storage itself, and frankly, it probably came a little bit more out of necessity in the beginning than—again, back to the—it looks maybe very clever in retrospect, but really at the time, it's like, "Well, our customers are scaling really fast; we don't really have time to look at a metadata layer." And it was just literally the metadata files were just JSON files on object storage that we were just doing CAS on, and it worked better than we expected it to do. When we needed a queue, we also just implemented it on object storage. Well, maybe we'll have to use a better queue at some point, but it kept scaling. I think this is also the bitter lesson of scaling infrastructure is that the simple thing often takes you very far. We kept learning that again and again at Shopify as well, but you keep being surprised too.

**Barr Yaron** [35:20]:

That makes sense. Look, you talked a lot here about trade-offs, right? Like the trade-off of, you know, we didn't even have time at the beginning for the metadata layer, and now we don't think it makes sense. But, you know, all of these databases, they do make some trade-off between latency and accuracy. Can you just tell me a little bit about how you measure accuracy? I know that turbopuffer does the automatic sampling of, I think, like 1% of queries to measure accuracy of index recall, but just a little bit more color on how you all think about that.

**Simon Eskildsen** [35:54]:

Yeah. So on the accuracy for recall, that's really important to us. I think that we didn't feel comfortable with just the academic benchmarks at the time. The academic benchmarks use dimensionalities that we weren't comfortable with, like in terms of the fact that we weren't seeing our customers using these datasets. A lot of the academic datasets are maybe 128 or 256 dimensions. Most of the production datasets we see have much higher dimensionality than that. The other thing about those datasets is that they don't have filtering. So if you filter by products that are on sale in Canada, it sort of cuts maybe half of the clusters in half. Then how many vectors are you supposed to look at to get good recall? I think it was just like this is where I think that we really feel like production—nothing tells the truth like production—and the way to tell the truth from production was to sample a small percentage of the queries, run it against an exhaustive search on the indexing nodes, and then just submit it to Datadog. In Datadog, we have a view of every organization and their recall, their p10 recall, and all of that. We spend a lot of time looking at that, and we look at query plans and everything. At some point, we'll for sure expose this to users, but it was the only way we felt comfortable that every query plan was going to have high recall in this production. So this has been a very important consideration in everything we've done for turbopuffer because we don't want our users to have to guess whether their search results are not what they want them to be because of inaccuracies from the search engine.

**Barr Yaron** [37:42]:

I mean, I love that. Look, you've lived in dealing with the mess of when things don't work in production. And so having a lot of empathy for that and making sure that things work as you expect. I mean, you know, we talked about—we alluded to talking about the optimizations and the changes to turbopuffer in the future. So, you know, it sounds like you've learned a lot from being very, very hands-on with this initial set of customers. So the first thing I'll ask is, at which point in time did you say, "We're ready for GA"? Kind of, "We're done with this." Like picking—you know, you said maybe you didn't know the words at the beginning, but picking the customers that at least felt right and working really closely with them. So when did the GA—when were you ready for the GA button to be turned on?

**Simon Eskildsen** [38:23]:

I forgot to answer the first part of your question before. So let's move back to GA, which is the trade-offs. So trade-offs are very important in a database. Again, if you go back to really the fundamental thing with databases and why some databases are good at things that other databases are not as good at. I spent so long being on the buy side of the database, and every single time I went to a database website, I'm just like, "Where are the limits? Where are the trade-offs? And where's the architecture doc?" Those are the things I care about. I just need to load this mental model into my head ASAP and know what it's not good at because otherwise, I can't tell what it's good at. The fundamental trade-off for turbopuffer is that the writes are slow, right? We have to commit them directly to object storage. It takes hundreds of milliseconds. So you're not going to do an OLTP workload on something like that; it just doesn't make sense. You can't do checkouts for e-commerce on something like that unless you get it all done in a single transaction. Then once in a while, you'll hit a node that doesn't have the data on disk, and you'll hit a cold query, and it will be a couple hundred milliseconds instead of tens of milliseconds. For search, that's a perfectly acceptable trade-off. For high-frequency trading, maybe not so much. We happen to think that this set of trade-offs are pretty phenomenal for a lot of workloads, especially search.

**Barr Yaron** [39:39]:

I mean, I really agree, and I think your website shows that fundamentally well. Like you can slide and understand how much you're paying; you can go and very clearly see what turbopuffer is good for, what turbopuffer is not good for. So that's very, very kind of customer-centric and clear, which I think resonates with the types of people you're selling into.

**Simon Eskildsen** [39:57]:

Yeah, I mean, I just wanted the website to be the website that I would have wanted. So on GA, basically, we just shifted when it was ready. All of our engineers spend a lot more time and gravitate more towards writing Rust than React. So I wouldn't say that it was like, "Oh, we were at this point in this curve and blah, blah, blah." We probably could have GA'd in January if we happened to GA what, like two or three months ago or something like that, maybe a little bit less. It was really just when it was ready. We hired someone who needed to maintain the front end because it had been me, but I got busy with a lot of other stuff.

**Barr Yaron** [40:37]:

Well, and they're deleting all of Simon's code.

**Simon Eskildsen** [40:40]:

Yeah, I mean, this is the thing now is that all the Rust engineers that are complaining about my code, they have to go rewrite all my JavaScript code, and they don't want to do that. So we're hiring some other people to go do that. GA was really about that. I don't think it was a maturity thing. I mean, there's always things that you want to improve in your product. But I think we feel really good about the offering that we have. Some of the things we wanted to do is that we wanted to scale a little bit of the support and go-to-market staff that we had to make sure that all of our customers are really well supported if they run into anything that we can help them with. But in general, there is no big brain game like move around when to GA or not other than this is when we feel that it's ready, and you know, it feels ready. That was at the beginning of this year where everyone felt very comfortable with going GA. It was sort of a question we asked each other monthly, and it's like, "Ah, we have a bit much going on right now." Around the beginning of the year, we were like, "Yeah, I mean, it doesn't matter; anytime." So I would say it was pretty vibes-based.

**Barr Yaron** [41:48]:

You know, we talked about first turbopuffer V1, turbopuffer V2. We could probably spend another three hours going into each of the optimizations, but if we just roll the tape forward and think about, you know, what does search look like in five years, and what are some of the demands as folks move to more agentic workflows, what do you see as the database needs of the future?

**Simon Eskildsen** [42:13]:

Yeah, I think to pattern match a little bit across what we're seeing from our customers, I think that what we're seeing is that the wave of AI companies that are doing well are trying to find more interesting ways to connect more data into their products to make the LLMs more useful. I think where we feel right now that LLMs are better than any person on the time frame that they could do is doing research on something. It is just phenomenal at this and generating reports over enormous amounts of data. What we see our customers want is just to search more data, and I think we can help them with that, and I think search will do that too. I think that a lot of search is going to be the LLM doing the search more so than the human. There's probably going to be a 100 to 1 ratio, something like that, I don't know, of agents and humans doing the search. But it's very clear that even if the context window goes to infinity, it's just going to be a lot cheaper for them to converse with the data in some way. It's never going to make sense to put a billion rows into a context window and ask it to do analytics on a dataset. It's never going to make sense to do ACLs in a context window. Recall is always going to suffer a little bit. So there will be some combination of this. I have no idea where search is in five years, but I think our customers have a really good sense of what they want us to ship in the next three to six months. We're very focused on listening to our customers and pattern matching across them and working very directly with them to figure out whether they really need something so we can make sure that we maintain simplicity in our product. So the long story—that's maybe the long answer to your question. The short story is we don't know, but we listen to our customers. They don't know either, but they know what they need right now, and if we continue to do that long enough in a principled way, I think that will serve them really well.

**Barr Yaron** [44:21]:

I love that answer. It's honest. Yeah, and it's working. One of the things that has come up throughout this entire conversation is this customer centricity. How do you hire the right team that cares about this and that is able to, I guess, engineer at the level of nitpicking your JavaScript code?

**Simon Eskildsen** [44:38]:

I mean, the short answer, I think that we just invite our customers into Slack channels, and our engineers too. I think our engineers take a lot of pride in the stuff that they work on, and then loosely will pattern match on, "Oh yeah, this seems related to something that I'm working on. I'm going to dig into this." I think we have a lot of trust in the customers that we work with that if they report an issue, that there's almost always something there. That mutual trust, I think, just shows, and it means that our engineers want to engage directly with our customers. So it's been a matter—it—I don't think I have any secret answer to this other than this has felt very natural, the way to build our business. It felt very natural that we needed to work very closely with our customers, and our customers really liked it. They've said things like, "We feel like you're a high-performing team inside of our company."

**Barr Yaron** [45:32]:

But are you screening for something at the door when you're interviewing candidates, when you're bringing people to come work at turbopuffer? Like, how do you balance that kind of cracked technical engineer with care about the customer? Are you looking for it explicitly?

**Simon Eskildsen** [45:50]:

I don't think I've met a p99 engineer who doesn't care about the customer experience. So it's not something that we screen explicitly for. I think we make it very clear how we think about our business. We don't have an interview session that's like, "Hey, you're on a customer call, and they're running into this bug. What are you going to do?" I maybe we should; I don't know, but I don't think it would be very high signal.

**Barr Yaron** [46:18]:

You don't know what five years out looks like; neither do your customers, and you're doing this. They know what they need for the next six months; you're adjusting and operating very quickly on that. Is there something that AI teams are doing today that you're confident is going to be considered bad storage hygiene in a few years, even if you don't know exactly what it will look like?

**Simon Eskildsen** [46:38]:

No, I think that some customers should be doing more bad storage hygiene than they do. We work with one customer, and they wanted to ingest a lot of data from third parties like Google Drive and others. I asked them, "How are you going to do ACLs? It seems very complicated to implement the Google ACLs." They're like, "Oh, like with your economics, we're just not going to deal with it. We're just going to have a complete copy of the Google Drive per user with the ACLs they have access to." Because that allows them to go to market quicker, and they'll solve the ACL thing later as an optimization. I think that's exactly the right way to think about it. I think that the pace that companies are moving at right now is faster than anything I've seen before. I mean, it's only reminiscent of the fastest pace that I saw inside of Shopify as I was going through the hyper growth into the 2010s, but it feels like so many companies are moving at that pace. I love working at this pace. To work at that pace, you have to make some of those trade-offs. I don't think there's anything that our customers are doing that's like bad storage hygiene. I think we see our customers run very fast with turbopuffer by abusing these economics to go to market quicker with product.

**Barr Yaron** [47:59]:

Yep. Yeah. And yeah, in many ways, that is the Jevons paradox that you want for now. So I will ask you one more question. And thank you so much, Simon. I'm curious how you think you've changed as a leader, as a person since you started turbopuffer.

**Simon Eskildsen** [48:13]:

This is a very good question. I think what it comes down to is that we have some very simple principles that we operate on as a company. Some of these we believe very strongly in, and we try to put them into everything that we do. I think that for a while, I didn't have as much conviction that these principles would work. Like, I didn't know whether being this customer-centric would work, but we've seen it work. So we're continuing to do it and directly working with the customers in a way that may be unusual. I think a lot of my growth as a leader has been to just trust that these simple principles, when applied for long enough, will do the job, and you don't have to sit down and come up with some strategy. It feels like a banned word at turbopuffer, right? That is not what we do. We have simple principles around how we do it, and we align on those principles, and you do that for long enough, and I think you can build a really, really great company. I didn't have the confidence for that two years ago.

**Barr Yaron** [49:15]:

I think a lot of what we had talked about is thematic with that, right? Like that first initial customer and following your intuition with Cursor, the Readwise aha moment without knowing exactly how big the market is, and things have compounded on top of that. So I know we're at time, but thank you so much, Simon, for coming on and taking the time. Always fun catching up.

**Simon Eskildsen** [49:33]:

Thank you for having me, Barr.
