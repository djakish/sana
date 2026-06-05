# Simon Eskildsen on scaling Shopify, building turbopuffer, and the future of databases

May 14, 2026•Cafe Cursor

[Video 3](https://www.youtube.com/watch?v=bWyOyyrVIXk)

## Transcript

**Sualeh Asif** [0:12]:

So you scaled a lot from 2010 through 2020. You know, what were the great sevs in Shopify history?

**Simon Eskildsen**[0:18]:

One of the funniest ones was we had this problem where about every hour, the primary or the writer of our MySQL clusters would stall for about 30 seconds and we couldn't figure out why. We were just debugging this endlessly and could not figure out what was going on. Someone figured out when this was going on, there was an `lsof` running on these machines. I was like, OK, why is this `lsof` running? And someone was, you know, tracing the kernel, figuring out, OK, this is causing a soft lockup in the kernel. Where is this `ls` coming from? It turns out that some of the Percona utilities, which are some of the Perl scripts used to manage MySQL, drew in PHP as a dependency. And PHP as a dependency has some standard cron job that every hour goes and does an `lsof` to figure out which files, which sessions are open, and then removes the files for sessions that are no longer actively open by the PHP process. And this was running every hour on all of our MySQL instances.

**Sualeh Asif** [1:23]:

What were some of the great systems that Shopify was running, which was at a scale that no one had run before?

**Simon Eskildsen** [1:30]:

I think Facebook had probably taken MySQL to really, really high heights before Shopify, but MySQL was certainly one of the systems that we were all — a lot of companies were just really compounding on the MySQL clusters through the 2010s. GitHub was in a similar situation. And so I think we just spent a lot of time scaling all the layers on top of MySQL. One of the big things that was more not novel, but a lot of the SaaS apps in the 2010s had been written in something like Ruby or Python or whatever, where you just had so many processes that couldn't do that many QPS per process, maybe in the tens, hundreds if you were lucky. And all of those processes had to have an individual connection to MySQL or Postgres. Postgres is actually worse at this out of the box. And so with MySQL, we had like 30,000 to 40,000 connections open, and so you're just spending so much time polling through all these connections and which ones you can operate on at any point in time. So we had this problem with Memcached, we had this problem with Redis, we had this problem with MySQL. Today there's lots of open source proxies in front of all of these systems, but at the time there wasn't really, so we had to play a lot of tricks to reduce this and just reduce the connection counts as much as possible.

**Sualeh Asif** [2:44]:

So how is the infrastructure team constructed so that you could handle like sev after sev after sev? And as you're scaling through this, what were the really interesting people that, you know, gel together to make all of this possible?

**Simon Eskildsen** [2:58]:

When I joined in 2013, it was still a very traditional structure where it was a pure ops team, right? People who are just incredibly good at operating Linux systems and SSHing into all of them, like sort of the, you know, servers as cattle era. The sevs that we had were not so much just the systems falling over. A lot of the sevs we had really came out of large flash sales driving enormous amounts of traffic into Shopify at one point in time.

**Sualeh Asif** [3:25]:

What's the worst flash sale ever?

**Simon Eskildsen**[3:27]:

I might not have been around for the one that overwhelmed the system the most, but I remember Kylie Jenner's flash sales as particularly challenging. I think she drove a lot of traffic even on trial accounts, like just showing up.

**Sualeh Asif** [3:38]:

So walk me through a normal flash sale. When there's a flash sale, how does it happen? What happens when a flash sale happens in the systems?

**Simon Eskildsen**[3:44]:

So a flash sale is generally someone with a very large following, and then through the 2010s we saw this — they might have like 10 million followers, 100 million, I don't know what's a lot of followers on Instagram, 10 million, tens of millions, and they have some new product they release. And so you drop the product and suddenly, you know, millions of people, or like hundreds of thousands of people, trying to buy the same SKU at the same time. And so that turns into an enormous amount of inventory lock contention on MySQL on that inventory row, and that was the kind of thing that drove a lot of outages. So we had to do a lot of things to make sure that both the inventory reservation and everything would work, because often you have hundreds of thousands of people fighting for maybe 10,000 SKUs. That was pretty much all the sevs, right? And so they would drive these sales and then they would just keep dropping things, right? Like Kanye would have a new sneaker, put it on Shopify, and then it would drive an enormous amount of traffic.

**Sualeh Asif** [4:43]:

So how are you guys preparing for this? You knew that there would be flash sales, you knew that the flash sales are getting bigger and bigger. How is the planning happening? How is the team getting ready for the next big horrible event? And the horrible event can happen at any point. It's not like, you know, for Stripe or something, there's like Cyber Monday or Black Friday, you know it's going to happen on Black Friday, but these were happening at random points.

**Simon Eskildsen**[5:04]:

Exactly. So these were basically random events that would take from, let's say, a thousand requests a second to a hundred thousand, right? So massive scaling events, and you're right, they could come at any moment. Of course, we also had to scale for Black Friday and Cyber Monday, but it was a little bit more predictable. The flash sales were sort of random tests and we didn't know — sometimes they came from a trial account. So the preparation was writing load testing tools. We had load testing tools internally trying to mimic what the users were doing. They were not particularly sophisticated; they were just little Ruby scripts that we ran on a lot of servers trying to serve inventory and contend on it, and then we would try to figure out what the bottlenecks were. That was the majority of the preparation we were doing. But this was also — the first flash sales that I was a part of were while we were still in data centers, so we had a team that was racking servers ourselves. It was before the cloud and so it was very difficult to manage how many servers that we needed. It's still difficult and I'm sure you're also now running into this at your scale because you actually have to go to the clouds. It's not infinite and you have to tell them how much you're expecting to use. And with GPUs, it's probably even more finite than in the CPU realm that I'm in now and that we were in then. But we were just trying to pad with as much capacity as possible. So, I mean, at the end of the day, you don't have that many choices when you have to scale very quickly. You can scale up. It's too slow. And on-prem, you can't really scale up much. The second thing is to cache harder. So larger TTLs and trying to move the caching up. In the beginning of Shopify, we were doing a lot of the caching in Ruby, but over time we moved some of that caching into Nginx directly and serving it from Nginx Lua to try to just move more and more load off the servers. We would also do things like shedding load. So if someone had a massive flash sale, we would try to prioritize requests, like if you had a cart or you had a session, you would prioritize that request over someone just coming to the site. So load shedding was another mechanism that we would do. You don't have that many other options other than that, and load shedding is just a way of trying to fail gracefully and failing fairly. Those were really the main levers that we had.

**Sualeh Asif** [7:11]:

How was the Shopify infrastructure organization organized? How did it — how was it in 2013? How does it go from like tens of people to hundreds of people or thousands of people now?

**Simon Eskildsen**[7:22]:

There was like probably five to ten people in 2013, like maybe a few thousand requests per second, and then we had a team of five to ten people that were engineers without an ops background. This is like when people just started talking about DevOps, right? Where DevOps was, oh, maybe someone can both SSH in and run the Linux commands and also write software. That was almost a novel idea back in 2013. And so that started to happen at Shopify. You had a bunch of people who were just doing performance engineering stuff in the application and a bunch of people doing servers, and those teams ended up merging. Back then it was really scary. It's like, well, you're going to have a developer writing Chef to configure the infrastructure, and that's what we were figuring out then. So those teams merged and we ended up calling that the production engineering team. I think Facebook did a good job pioneering this pattern and then it just started breaking into different teams.

**Sualeh Asif** [8:13]:

So this is actually really interesting. One of the things that I've learned is that actually it wasn't just, you know, there was a bunch of great companies scaling in 2010 — there's, you know, early Stripe and early GitHub and early Shopify and so on. There's like tons of different companies and they all collaborated with each other. How are the teams, like different teams, you know, infrastructure teams collaborating across the organizations to build tools? What are the great tools that came out of it? What are the great open source libraries that people use now that have stories in the infra wars of the early 2010s?

**Simon Eskildsen**[8:45]:

I was just talking to — we probably both know Sam Lambert who runs PlanetScale. We were talking about this, how in the 2010s a lot of these lessons, and it's probably still the case, are not written; they're shared on phone calls. Like you and I had some of those phone calls in the early days of Cursor, like how do you do this and all of that. And it's a lot of wisdom that honestly the models can't really train on because most of this is just in a bunch of people's heads. And in the 2010s there was a bit of a collaborative intelligence between these companies.

**Sualeh Asif** [9:13]:

Walk me through some of the people you talked to.

**Simon Eskildsen**[9:16]:

So we talked to Zendesk, who were also scaling Ruby and MySQL. Intercom was also using a bunch of the libraries that we built for caching and also Rails and MySQL. GitHub, of course, GitHub built things like this open source library called gh-ost, which is something that uses the MySQL bin log to run migrations of the schema. SoundCloud built this thing called LHM, which was a way to use triggers in MySQL to do schema migrations. At Shopify, we wrote a project called Toxiproxy. I don't know, have you ever heard of this project? Yeah. It was a project I started because everything just started failing with all these different services that we had and we needed a proxy where we could guarantee that if we took down all these different services that things wouldn't fail. So for example, we had a database that was managing all the sessions and carts and we needed to ensure that if that database failed, all of Shopify stayed up so we could write tests against it. I think a bunch of those companies used that. I think Sam and I, like at GitHub and Shopify, we were on the phone together and we're like, oh, what are you doing about this with Ruby and how are you scaling MySQL? And is this proxy good? And what are you doing about connections? And, you know, we would send patch files together to get Redis to scale better. And there was just a lot of this collective wisdom, a lot of these Ruby on Rails MySQL shops in the 2010s.

**Sualeh Asif** [10:35]:

What's the story of Logrus? Logrus is probably your most popular library?

**Simon Eskildsen**[10:43]:

Logrus is a Go library, and I created it because I was in so many incidents where I was just so mad at myself from six weeks ago for not more intentionally thinking about what output I wanted to see from the system at a particular point in time. And so I wanted an API that just forced me to sit down and think about what information I want to dump out. So the Logrus API is like `logger.WithFields` and then you have to type them all out. Now that's really the only thing that's well designed about Logrus; it does way too many allocations and I haven't had time to actively maintain it very much, but it took off.

**Sualeh Asif** [11:22]:

What does it look like when a library takes off?

**Simon Eskildsen**[11:25]:

I think it has like 25,000 stars and I just kept finding people kept telling me, oh, we're using Logrus, I really like it. And so people got the idea of just structured logging — this was before OpenTelemetry and all of that, so I think it just clicked for people.

**Sualeh Asif** [11:43]:

So one very interesting part that you mentioned in the Logrus story is, you know, you want to write things intentionally, be very careful, be thoughtful, like in advance when you'll actually need to be thoughtful. What are other great engineering principles? What are the Simon engineering principles that one writes in an `AGENTS.md`?

**Simon Eskildsen**[12:04]:

The way I think about software in my head is that over time, the software has to age well. And over time, the software is under strain from time, patterns changing, the language changing, scale, and lots of people working on the same thing. The only thing that I just keep coming back to, which is a major inspiration also for turbopuffer, is just to make it as simple as possible. I worked at Shopify for almost a decade, and it's rare these days to have worked in one code base, one company for that long. And what it taught me was how software ages, because I saw so many projects where people would spend a long time writing an RFC and doing a big project on something, and that was not a predictor of that software aging well. And then I saw sometimes where on the infrastructure team, we just had to hold something together with spit and bubblegum, as my boss used to say, and it would age phenomenally. Like that spit and bubblegum would be perfectly in place and holding water five years later. And so I think the biggest thing that I just learned is just to keep that simplicity — they surprise you and complexity has to be deserved. That's the underlying principle, I think a lot of things follow from that. I spend a lot of time thinking about how different things are gonna fail if you 10x or 100x the scale. Like if you're doing any infrastructure change, it has to be able to scale 100x, otherwise there's no reason to make it. You're just playing musical chairs. But the aging well and letting simplicity start and complexity be deserved is extremely important and it's fundamental to how we've designed turbopuffer. The other thing is that being on call on the last resort pager of a piece of software where real people around the world are losing millions of dollars per minute of downtime is an enormous responsibility. We were maybe six to eight people on that pager. And if you got paged, you knew it was on you to bring the site back up. And I think that taught me how to write software in a way that — it changes you for better or worse, right? And so with turbopuffer, for example, it was just like, what's the piece of software that I'm willing to go on call for? And it has to be extremely simple and very easy to debug. And back to Logrus, right? It's how do we make this as easy to debug as possible? Because every line of code that I write is a liability for someone to be on call for at 3 a.m.

**Sualeh Asif** [14:35]:

Well, this is super interesting. I guess one way to frame the question is, you know, as you are RL-ing the models, you kind of are writing a constitution that the models have to follow, because not only are they trying to, you know, get better and better at passing a certain suite of tests, but also we want them to write good quality code, you know, in the same way that humans have this constitution that, you know, it's very simple, it's not very complicated, but it's very widely applicable. Is there a software engineering constitution that we should be using to train all the models such that the code that comes out is good quality, stands the test of time? I think a lot of people generally are worried about slop that the models produce. We kind of want to train them to not produce slop. What's the constitution so they don't produce slop?

**Simon Eskildsen**[15:22]:

When I talk to the models about designing software, it feels like they want to design systems like an eager undergrad who's read way too much Hacker News. I would ask you how you would RL a model to encode principles like that of just simplicity over everything, because it's a certain set of trade-offs, right? Like in turbopuffer, we keep everything on object stores, there's no state anywhere else. And it's like, yeah, if you want low latency for writes, you gotta look somewhere else because we're not going to give you that because I'm not willing to accept the trade-offs. I don't know how good the models are at navigating trade-offs like that. It feels like they're very eager to design a very perfect system and not that eager to try to design something that's very simple and will age well under those kinds of pressures. But how can you RL that?

**Sualeh Asif** [16:07]:

I think one can come up with various ideas, right? By default, you're trying to — even just the thing as like, write something as simple as possible, and when you're judging between two correct solutions, prefer the solution that's really simple. And, you know, another one that you could try to encode for is make sure it's short. You know, if you enforce the model to produce short things, minus the code golfing aspect, shorter things are generally simpler. Don't write something in a hundred lines that you can write in ten.

**Simon Eskildsen**[16:37]:

But I don't think it's always about the lines of code, right? Like the lines of code sometimes of, you know, something like turbopuffer might accept all of its writes into Kafka or something like that. But now you're operating this whole other system that might be less lines of code, but the system complexity is a lot higher. Do you give the models a constitution?

**Sualeh Asif** [16:54]:

Yeah, we actually write down the things that we think of as great, great code. And that's important because if you don't write such a thing, the models will write all sorts of crazy stuff that is unbelievable.

**Simon Eskildsen**[17:05]:

Yeah, I think also just thinking about all the graceful failure modes, right? And just the recovery of the software is also something that we've always tried to preserve — the property that you can shut down every single server and you lose nothing, like there's no lost data. And it's surprisingly difficult an invariant to uphold. And so I think the other thing I think about with software aging are just what are the invariants in the system that I care about, right? Like we've both done competitive programming, right? And you're always thinking about what are the invariants in the system that has to hold under every condition, because otherwise you know that it's going to fail some test at some point.

**Sualeh Asif** [17:38]:

You write a famous blog. Famous-ish blog.

**Simon Eskildsen**[17:42]:

A stale famous-ish blog maybe.

**Sualeh Asif** [17:44]:

What's the story of starting a blog? It was very, you know, inspirational for me when I read it back in the day where it's like — for people who don't know, the blog basically walked through, you know, for seemingly complex engineering problems, how can you break it down into Fermi estimates and actually get a, you know, fairly reliable and good estimate of what the system will perform under weird, weird shit that is otherwise really hard to estimate.

**Simon Eskildsen**[18:10]:

I had no idea that you read it. Did you read it before we got to know each other? So what you're talking about is napkin math.

**Sualeh Asif** [18:16]:

A bit, yeah, a bit before.

**Simon Eskildsen**[18:18]:

Napkin math came out of my role at Shopify where I was a principal engineer. And one of the things you do as a principal engineer is you're reviewing a lot of the science. So you're reviewing the science of "I want to build this product, it's going to use the database in this way." And something that I found myself repeating a lot — people would come to me with a benchmark and say, this is how I expect this database to perform. Like I still really don't like benchmarks very much, especially not when you're trying to make a technical argument, because benchmarks are like a point in time. They don't tell me anything about what the fundamental properties of the system are. And so the lesson that I felt like I was preaching in so many of these reviews talking about like, okay, this might be the benchmark, but the fundamental properties of the system are that, you know, if you're trying to do a search query, for example, you can just do a little bit of math and figure out how many gigabytes of data that you have to move to serve the query. You see, okay, to service this query, I have to move around a gigabyte of memory; DRAM can maybe do about 10 to 100 gigabytes per second, so this should take somewhere between like 1 and 10 milliseconds. And you would come back with a benchmark and they're like, well, it takes five seconds, so we can't use this database. And it's just an unacceptable explanation to me because there's a gap here between my first-principle understanding of the system and my dumb high school multiplication math and how the system is performing. So that gap between the first-principle understanding of the system and how the system is actually performing in the benchmark — we have to close that gap before we can conclude anything. And that gap is either, like, my stupidity, like this math is wrong, or it's that this system is not performing, but like one of us is wrong. And unless someone could close that gap, it was just like, you're not making a compelling argument. The benchmark is not persuasive unless you can explain that the benchmark only tells you how close you are to the theoretical floor. And so I just started a blog to try to explain this and to give myself a bit of a cadence of like, okay, you know, you're trying to do this join, how long might that take? And then just doing a bunch of math, running the actual test and seeing what the difference is, and then trying to reconcile that gap.

**Sualeh Asif** [20:39]:

What's your favorite blog post in the series of blog posts you wrote?

**Simon Eskildsen**[20:43]:

I really like the one about TCP windows. Have you read that one?

**Sualeh Asif** [20:48]:

It eats away at every single, you know, scaling system.

**Simon Eskildsen**[20:52]:

It does. And it became very important to winning a very important deal at turbopuffer, actually, at some point — this blog post. So this blog post is basically — this was a prompt that came to me at Shopify where someone was like, okay, why does it take three seconds to load a page in Australia, right? So you're going from Australia to US East. I'm guessing that round trip is probably 250 milliseconds — there's like a pretty good undersea cable probably over the Pacific and then you're running the 60 milliseconds cross continent, probably around 250 to 200 milliseconds, right? But the page load was taking three seconds on like a vanilla Shopify store. You just spin it up and like I know that the Ruby and stuff is taking like 10 milliseconds. Makes no sense. And this just haunted me then. And then I went to visit the site and I would refresh and it would take less time, but it was not a cache hit. I would skip the caches and then it would take 260 milliseconds. And just like, what is going on here, right? There's like three seconds versus 260 milliseconds in my understanding. Like what is this gap? Is it my stupidity or something like, not working here? And so I dug into it and spent a bunch of time in Wireshark looking at it. I'm like, why is it going back and forth over the Pacific so many times? And it turns out that in TCP, what you do is when you open a connection — so, you know, Sydney dials to US East — US East will only the first time send 10 packets back because they're trying to negotiate how big the link is between the two and it has a very conservative default. So it'll send 10 packets. The packets are like 1500 bytes each. And so a website that's like 15 kilobytes is going to load faster on most machines than one that's 16 kilobytes. And I just sort of deduced this from Wireshark. So this website is in like hundreds of kilobytes, so it does 15 kilobytes and then TCP says, oh, okay, I guess the link is big enough for 15 kilobytes. Let's try 30 kilobytes the next time. So now you transfer 45 and it keeps doubling. But now you're doing a lot of round trips back and forth to negotiate the size of that link. And so what I realized is, okay, well, if you just tune the Linux kernel's TCP settings on both ends to send 100 packets into the first round trip, this will go a lot faster. The downside is if you have a very, very bad uplink, you're going to lose a lot of packet loss, but in general, it's probably a better setting. And this became applicable even at something like turbopuffer at some point.

**Sualeh Asif** [23:05]:

So that brings us to one of my other favorite questions. So there's a two-part database question. So number one, what are the great databases of the past? You know, what are the systems you found very inspirational? What have you learned from them? Walk me through your understanding of the evolution of databases.

**Simon Eskildsen**[23:25]:

So there's like I think there's a couple of angles to this. The way that I think about it is that about every 15 years, the ingredients are in the air to build a new database. A lot of databases come around, right? I'm sure there's thousands of databases in production around there, but in terms of big databases, it sort of begs for a new platform, right? Like Oracle, for example, and MySQL and Postgres are built in the 90s. We have the web, and we have a lot of all these SaaS companies being built and lots of data going into databases. That was the first, I would say, wave of big production databases. There were some in the 80s, but this is — 80s and 70s are so far before I'm born, so I don't have a great understanding there, but obviously there are databases there often on mainframes and so on. But the 90s was when the first grade of big database companies were built. Then about 15 years later, you have a new workload, not just, you know, websites and so on trying to store data and applications starting to store data, but you had these large-scale OLAP workloads. And so then you have Snowflake and Databricks and a bunch of other companies being built around this new workload. I think big database companies basically are companies where every single company on earth has data in that database either directly or indirectly. And so, you know, while the proverbial textile manufacturer in Bavaria, Germany is not going to go out and buy Snowflake directly, almost certainly they're using some product that's using Snowflake or Databricks, right? Or tens of times. So their data is probably in that database tens of times. And those are how the biggest database companies are built. And the biggest database companies — I think now there's a moment where that's happening again, where there's all these AI workloads that are being trying to be connected to data. But that's one way I think to see it in the past. In terms of inspirational databases, SQLite is the first one that comes to mind for me. What I really like about SQLite is they just have this hardcore minimalist philosophy in everything that they do. I think the best example is, and this would go in my software `AGENTS.md` constitution, is to try to get as much pressure on every code path as possible rather than having separate ones. In SQLite, I have a phenomenal example of this. And it's that normally when you do a join ad hoc, you will do some version of like a nested for loop or a hash join or something like that, and you implement that as a particular path. SQLite basically will construct an ad hoc B-tree in memory to perform the join, which is the same code path as far as I understand of the B-tree index they build when you actually build the index on disk. So they're putting more pressure through that single code path, more optimization yielding to more and more query plans. I think that's brilliant. Obviously, another database that I admire is like Google Cloud Storage and S3 and these blob storage where they have very, very few APIs, but those APIs have a very, very consistent histogram of latency, almost infinite scale, and they work and they're extremely reliable. I really like systems that have very few primitives and you just know they work and you know that they will honor their histogram bounds even when subjected to a lot of time, like so reliability, but also a lot of scale. So I've certainly taken a lot of inspiration from that as well. For me, there's also a bit of nostalgia even just in the old days of just ripping it with FTP and phpMyAdmin and a MySQL box. I really like that and there's a sense of that that I would love to bring into the database that we're building today.

**Sualeh Asif** [27:05]:

So next, there's part two of my two-part database question. One of the things I don't understand very well about the database industry — this might sound naive — but most systems that you see in the world have this property that as the industry matures, there's one winner, and that one winner just stays the winner forever. So just to walk through a few examples: operating systems, there's a lot of competition, then there's Linux wins in some ways, or for consumers, macOS wins, and that just stays the way forever. And, you know, for virtualization, there's one winner and that winner stays the winning company forever. And like almost all systems have this property, you know, it's true for OSes, it's true for anything you use in terms of APIs in the real world. Even in clouds, there's like basically AWS plus two copies and those are like the standards forever. There's no real new contender propping up every 15 years of disrupting a standard. A database is the only example where every 10 years there's like new companies every year, like someone starts a new database in the hopes of, you know, becoming the next big database that everyone will use. And there seems to be this consistent innovation over a period of, you know, half a century, which it doesn't feel like anything else has. You know, there are no great infrastructure companies where once the infrastructure company starts winning, it stays stable. There's like a new one in the category every five years. People joke to its death that the only other answer to this is JavaScript frameworks, but outside of JavaScript frameworks and databases, why do databases have such a property?

**Simon Eskildsen**[28:52]:

So the mental model I have of this is that to serve new infrastructure software, there needs to be a new workload. And so the workload that we expect of an operating system hasn't changed enough for us to require a rewrite, right? We expect now virtualization and really good isolation primitives from the operating system. Solaris was a lot sooner to that than Linux, right? Like Linux only really got good at that even just a few years ago, right? And they were working on that through the 2010s, but it wasn't enough to disrupt the whole thing. And so I think with databases, we do see new workloads like every 10 to 15 years, but there's not as many new workloads as I think people think that really matter. Now, there's a lot of niche workloads, right? So there's a niche workload in graphs. It's a fairly niche workload, but the big database companies are built when you start with a sensibly niche workload and then expand from there. I think MongoDB is a great example of that, right? They started with very web, like get up and running very quickly and then they've just expanded — they do time series, they do graph, they do everything. So I think that's one property is workload, but workload is not enough.

**Sualeh Asif** [30:01]:

Why can the incumbent also get really good at that workload? Then why can't Oracle just keep getting better and there's never a new database company, but Oracle v1, v2, v3, v4 — it seems like they get disrupted again and again and again.

**Simon Eskildsen**[30:05]:

Yeah, so that brings me to point number two. So you need a new workload because there needs to be some reason for every company on earth to have some data in this new database. Otherwise, it's not going to get that big. The second thing that you need is a new fundamentally new storage architecture that is advantageous for that workload that the incumbent can't get to. So an example of that, right, is like Snowflake and Databricks are architected with commodity hardware, either, you know, directly on object storage. And Oracle can't do that, right? It does not have a separation of compute and storage, so it can't do these massive OLAP workloads. Even though you could ostensibly do that as an extension of the design, Oracle has at that point what, 30 years of heritage of assuming a tight-knit, non-separation of compute and storage. Now, right, we can build databases that take advantage of NVMe SSDs, and Snowflake and Databricks didn't do that because NVMe SSDs weren't even in the cloud until like eight years ago. And NVMe SSDs require you to architect the database fundamentally differently to take advantage of it. You need a lot of outstanding IOPS in very few round trips. And that's also what you need to do to do very low latency on object storage. The other thing is metadata. So the first generation of databases built on object storage would have had to put all the metadata in a separate database, which has all kinds of other problems like running on other people's clouds and running in many regions. And you can build databases only as of like a year and a half ago that can have the metadata also in object storage. So when you have those two conditions of a new workload — in the 90s, it was, you know, computers, internet, and 15 years ago was OLAP with analytics. And today it's connecting very large amounts of especially unstructured data to AI. That's the new workload you need. The second thing you need is a new storage architecture. It can't be copy, right? The search engines before turbopuffer can't copy it because they tightly coupled compute and storage, and Oracle couldn't easily copy what Snowflake and Databricks did, which was separate compute and storage.

**Sualeh Asif** [32:21]:

What's the story of Simon becoming a fan of databases? You're clearly very passionate about databases.

**Simon Eskildsen**[32:28]:

I love databases. I think competitive programming has a lot of just database-adjacent topics and so you just start thinking in asymptotic notation and how things are executed. And when I got to Shopify, I was just so drawn to the people who were working on databases. And the thing that breaks when you scale a website is generally the database, so it's just always the thing that was breaking. And so we have to stay ahead of it not breaking tonight at the flash sale and not breaking a year from now when the flash sale would be 10 times as large. So it just becomes a fundamental bottleneck for scaling and that just drew me to it — like why these things are hard to scale. And so we just have to keep working on it and working on it and working on it. And at some point my model just started shifting from thinking about it as a thing that executes SQL to just thinking about how the bits and bytes are laid out on disk. They have so many fascinating trade-offs, right? Like we just talked about storage architecture being different for different query workloads. There's so many trade-offs in databases. And I love thinking in trade-offs. Like I love thinking, okay, well, if we do this, it's going to be better at this, but worse at this. There's just, especially in search, it's like applying so many parts of computer science and computer engineering into one particular domain, that there's just an infinite amount of fascinating problems.

**Sualeh Asif** [33:44]:

I remember the first time we met, we were — at Cursor we were running into some Postgres bottlenecks. And one of the most fascinating things at the time was you laid out, okay, here's how Postgres is architected. I can like make a simple mental model of like things in Postgres happening this way and things in MySQL happening this other way. And at the time, even though I had read about Postgres and understood some of the architecture, I hadn't mapped it down to all the little blocks how they interact with each other. How'd you learn that? How does one learn about the weird intricacies of, here's how Postgres is designed in all of its little bits and components and all the query parameters that one can find around for those things?

**Simon Eskildsen**[34:25]:

The short answer is that I can't help myself. I don't think I'm particularly smart, so I just spend a lot of time trying to dumb it down to something that I can understand and explain to other people. And I spend a lot of time on a little notebook and just trying to draw out exactly how did the blocks move around. And so I think it took me about 10 years to get a very, very good understanding of how MySQL works. And then when I left Shopify, I was helping some of my friends' companies scale, and I kept running into Postgres and I was like, okay, this is cool. Like, you know, I don't know anything about Postgres, so I just sat down one day and I just spent eight hours reading the entire manual with a lens of how is it similar and how is it different from MySQL. And just always comparing and contrasting. So it was easier because I already had sort of a trunk to land the knowledge on. And so the compromises became very apparent, right? The way that the indexes work in Postgres is very different from how they work in MySQL, right? In MySQL, the way that the data is laid out on disk is dictated by the primary index. So at Shopify, we took advantage of that by having all of the data for a shop located together. That's very complicated in Postgres. Postgres handles writes in a very different way that requires a lot of tuning for the user that MySQL doesn't. And I mean that was the problem you were running into when we met.

**Sualeh Asif** [35:45]:

How do you code with AI? Or in general, how do you use AI? Where, is there any way where models are helping you outside of coding?

**Simon Eskildsen**[35:53]:

For sure. So, on coding in particular, I just have a Cursor window with a website open. A lot of what I do is like docs or smaller changes. And I just, yeah, I just have Cursor running. I use a synchronous agent and then I just choose a model based on what I need to do. If I just need to answer a question about the code base, I use the Composer model because it's very fast and searches a lot. And then I use different models depending on the kind of task that I'm doing. But generally I'm managing a few agents inside of Cursor. And it's especially inside Cursor because it allows me to make it very easy for me to review the code. I think my contrarian view is that we're still going to be reviewing every single line of code that goes into the database by the end of this year because that seems paramount still for the database. Like we have a couple of individuals at turbopuffer whose job it is to just have the entire context of the code base in their wetware neural network, and it still works really well because they have their manifesto and `AGENTS.md` also embedded in that wetware, and they make good local optimum decisions and the agents help them.

**Sualeh Asif** [37:00]:

One of the things I've been excited about over the last many months is getting cloud agents to be better and better and better. What would it take for 50% of turbopuffer's code to be written by cloud agents? And I imagine one of the bottlenecks of writing 50% of your code is you really want to be sure that it's verified the thing, you know, all sorts of queasy conditions. How do you think about testing in the age of models operating for 12, 24, 48 hours? And in general, at what point does 50% of turbopuffer's code be entirely written by cloud agents?

**Simon Eskildsen**[37:39]:

I think we probably should have agents running all the time trying to break the products before we do or before the customers do, and use cloud agents. I'm sure some of the engineers are already doing that. I mean, generally, when I use the model still on core, like core, core Rust and database things, it's still not making globally optimal decisions. So I feel like the level of when I'm doing something in the dashboard or on the website, it can get such lax instructions and do incredibly well. But in the database, there are just so many other properties of how is this API going to age, right? Like how is the data going to be laid out? And there's all of these war lessons, right, that the model has not learned about how to operate this. What has to be true? I think the model just has to get better. I don't think I have any more wisdom on that. And I think that I want the model to talk more about how something is going to age, how the storage, how this is going to make what irreversible decisions on the storage architecture — like ostensibly irreversible decisions.

**Sualeh Asif** [38:44]:

So three turbopuffer questions. So first one, what would it take for turbopuffer to become Google scale?

**Simon Eskildsen**[38:50]:

To index the entire web? It can already do that.

**Sualeh Asif** [38:54]:

Yes. Index the entire web and serve the QPS that, you know, Google would serve. Like actually be usable by Google.

**Simon Eskildsen**[38:59]:

So, I mean, I don't really know — Google, I'm sure, is a web of like a thousand services to do it, but we have customers that have indexed the entire web into turbopuffer and it works and it can do thousands of QPS. Now, I'm sure Google has their hands on more than the hundred billion or so documents that we've procured in the data sets that have been indexed into turbopuffer, maybe in the trillions, but it can certainly be done. There's no reason why that wouldn't continue to scale. And we've done the hundred billion with p99 of 200 milliseconds and p50 of like 40 milliseconds, so that's quite possible. Now, to get the relevance to where something like Google is, you might have to do a lot more, and that would be quite a few iteration cycles. But I mean, you can index the entire web with not that many servers with the turbopuffer architecture.

**Sualeh Asif** [39:43]:

Could it ever be built without S3? Is S3 sort of being super strongly consistent just one of the great engineering feats of the last decade?

**Simon Eskildsen**[39:53]:

Yes, I think so. Back to the original question — the question about why there's new database companies coming up every 10 years is that we needed NVMe SSDs, which were not available in the clouds until like the late 2010s. We needed S3 to be consistent, which did not happen until December 2020, which is mind-blowingly late. And then we need compare-and-swap.

**Sualeh Asif** [40:13]:

Why is it so hard for S3 to be strongly consistent?

**Simon Eskildsen**[40:15]:

I don't know. In Google Cloud Storage, I think it's probably Spanner or something sitting in front. And so that's a little bit easier for me to understand. There are more or less no details on the S3 metadata layer. That metadata layer is presumably very difficult to operate on. And I know that S3 — yes, their API surface is small, but I know they invest a lot in formal verification and all these different things, presumably because they have little bugs that people rely on and they have to make sure that even when they change the tiniest thing, that doesn't break for a lot of other people. And so I don't know what makes it so difficult. I think probably the thing that makes it most difficult is that the system existed for 15, maybe 20 years without having that. And from the systems that I've seen, it's a very fundamental assumption in the system and you're going to be engineering for 15 years assuming that this is not true. Yeah, that would probably take — I'm sure that took them like five years to do. I'm guessing Google Cloud Storage, because they were fronted by Spanner, might have been consistent much earlier, if not from day one. But I also know that they consider that one of the greatest mistakes of S3 that they weren't consistent from day one.

**Sualeh Asif** [41:22]:

What else do you like about S3? When I say S3, I guess I mean...

**Simon Eskildsen**[41:26]:

Big storage. Yeah. I think just the simplicity, right? Like it's tight latency bounds on very few things. I think it's very, very predictable systems. It automatically shards and it's infinite. turbopuffer would not exist without it, right? Like if this was like 15 years ago, we would have people full time just racking HDDs and trying to strike deals to get as many HDDs as possible. And we'd have the problem that you probably have with GPUs, but just with HDDs. I'm very happy to not have that problem. I don't think turbopuffer would exist. And I probably wouldn't have started the company because I would not have wanted to be on call for a product that required racking HDDs and flying to Ashburn every week to rack more HDDs.

**Sualeh Asif** [42:10]:

One of the really weird things I've had recently is it's getting harder and harder to interview people, where one of the great interviewing tricks of the last decade was instead of interviewing them on the blackboard, give them a really complicated code base and let them try to do something in a complicated code base because naturally doing things in complicated code bases requires an incredible amount of RAM. Like you need to be able to hold a lot of things in your head while still being able to produce net new stuff other than just getting, you know, stuck or being asked to deliver, you know, day to day. If you need to deliver in two days something that is required of you, you can just shut everything else off and actually focus on the thing. Sadly, language models have just gotten so good that you can't use that interviewing trick anymore. How do you think we go back to first principles and interview great engineers? Because, you know, there's obviously drawbacks of doing things like whiteboard interviews because whiteboard interviews are not the thing you're doing on a day-to-day basis — isn't exactly the skill that you're testing.

**Simon Eskildsen**[43:15]:

This is something I spend a lot of time thinking about. I have a document called Traits of the P99. I don't know if I've ever shared an early draft of this — you and I have spent a lot of time talking about interviewing over the years. And it's just a list of traits of the P99. And it's a long list and I can't share all of them because that would be too easy. But I think that the P99 is someone that you would describe as fast. It comes out in very, very different ways in a lot of people, right? You and I can talk very fast. And so that can be interpreted as fast. But some people are fast because they move very deliberately and there are not bugs in their code, and they just keep moving forward and it's just one step forward, one step forward, and it's never two steps backwards. It comes out differently, but I've never met a P99 that I could not in some way describe as fast. Another trait of the P99, I think, is that they have bent something to their will, whether it's software or their trajectory or something like that. They have just made the machine or something do what — I mean in Silicon Valley you call this agency, but it's also agency or facility with the machine itself to get it to do what you need to do. I think P99s try to the best of their abilities to surround themselves with P99s, and they probably have multiple moments in their life where they discover that there was another level and they could not help themselves but to try to get there, right? You're an immigrant to the new world like myself, right? And so you probably at some point tapped out of the P99s in your local community and you went looking somewhere else, right? And I think one of the interviews that we do is an interview that we call the life story, and our recruiter Jen spends an hour and a half with people just going through their life, like all the way from when they first were introduced to something that they were excited about. And I think that the P99 got very excited about something very early and they continued to seek out the next nine, like P99 to P99, right? And for you, I'm sure it was like going to MIT, you discovered another level, right? Being an IMO, you discovered another level. And I think that's another trait to be looked for. And so I just have a list of these — like they're obsessive, like they can't help themselves but get in the weeds on some detail they find particularly interesting about something. They just can't help themselves. They don't remain at this level, like it's not like a monotonous abstraction level. They can't help themselves but go up and down. And so I have a list of these and I just look at them after I've interviewed someone — after I talk to someone, I have a particular interview that I do where I look for that. What do you do? What do you look for now?

**Sualeh Asif** [45:57]:

Actually, I've been thinking about this a lot. I mean, I don't have a great answer. I do think probably we should go back to logic puzzles where I think there's something that comes out when you go into something like these weird probability questions, where probability questions require something like clarity of thought. In periods of stress, having clarity of thought is more important than one realizes. And of course, a probability question itself, you know, can be easily solved if you're, you know, very well versed in probability, but also just being able to stay calm and then say, we'll reason through this together. So for most people, it's actually a bonus. Like the whole reason to pick a probability question is to pick for people who, you know, don't have probability experience. It doesn't have to be probability in general. It could be anything where you're like — I've heard of various questions where you come up with scenarios and the scenario is something that is rather tough and the problems get tougher and tougher over time. And it's just really interesting because, you know, if it's very easy to produce code, probably the thing that matters is can you clearly think through the system, and then can you clearly articulate what you want out of the system and what you don't want out of the system? What are the invariants you care about? So all of those things — like back in the day, if you had to trade off between a philosopher and, you know, someone who's a workhorse coder, you could probably pick the workhorse coder because they had refined their skill at Next.js or something. But actually that's become less valuable over time, so you probably want people who are just incredibly thoughtful. I think one of the things you mentioned that I think is really underappreciated is just always having the person who's one step forward and never two steps backward is vastly underrated in the world. I think those people are just very, very, very calibrated and you will always trust them on the hardest problems in the company and that kind of skill, even though it's hard to interview for, is pretty important.

**Simon Eskildsen**[48:02]:

I think one thing that is probably the best way that we try to figure out if people have clarity of thought is to ask them how did they design a system and then see how they ratchet up complexity and navigate the trade-offs, right? That clarity of thought of like — you're going to start with the simplest thing and then you're going to attack it with some scale or whatever, and then we talked about like the serving complexity earlier, right? Like the system you — sometimes engineers often gravitate towards just designing the perfect system that doesn't really have any trade-offs, but that's not one step forward, one step forward. That's like trying to take 10 steps forward in the first step and you're going to fail because you're going to make an assumption that's wrong. I think the P99 navigates that ladder of complexity extremely elegantly.

**Sualeh Asif** [48:45]:

What is the next frontier of databases? 15 years from now, what's the next frontier? Do you think we have the last database?

**Simon Eskildsen**[48:53]:

Definitely not. There's going to be hardware advances, right, in the next 10 to 15 years.

**Sualeh Asif** [48:58]:

GPU databases?

**Simon Eskildsen**[48:59]:

It might be — like people have tried that, but I don't think it's quite ready, right? But it would make sense to me that the GPU becomes more and more general purpose, could do more and more instructions and can move more and more bandwidth. So that might be the next platform, right? Does it take 10 years again? I don't know. The GPUs are evolving very fast. So I think that's one go at it. I think that it's been a pretty consistent pattern that machines — like this has happened in CPUs, it's happened in disk, and now it's happened on object storage — where a lot of speculation. So basically having a very large amount of outstanding requests at once in few round trips, that's been a good bet. And that's also how GPUs work, right? You try to give them a lot of things to chew on at once, get it back, and then do something else conditioned on that. So like making everything good for speculation and predictability has worked out really well.

**Sualeh Asif** [49:47]:

Would you be surprised if we just have the last database? We won't need more databases in the future. I actually think — there's like a controversial opinion — I think turbopuffer and, you know, OLAP plus Postgres is probably combined, or like the levels of scale that we found to hit them — maybe not Postgres, but something like Vitess plus massive scale — I think we've hit everything we need. I don't think we'll need another one.

**Simon Eskildsen**[50:11]:

I would hesitate to say never. I would love for turbopuffer to be the last database.

**Sualeh Asif** [50:16]:

We'll call it a day. Thank you so much for, you know, coming by the office. I feel like I've learned a lot from you over time, both in, you know, architecting great systems but also, you know, building high-performance engineering teams. And thank you for being a great partner to Cursor also. There are, you know, so many points of Cursor where life would have been very hard if we didn't have Simon to rely on. And yeah, thank you so much for coming down here.

**Simon Eskildsen**[50:39]:

I really appreciate that. I mean, you know, we've had many conversations on the phone about infrastructure and you have a good enough team now that you don't call me anymore. But I do kind of miss it. And it meant a lot to me. And it means a lot for you to say that because you've built a great company to have been a very, very small part of. It really means a lot.

**Sualeh Asif** [50:57]:

Thank you so much.
