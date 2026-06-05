# He built a new database in his bedroom

October 30, 2025•The PMF Show

[Video 3](https://www.youtube.com/watch?v=KKyhFl-7uPI)

## Transcript

**Simon Eskildsen** [0:00]:

I was thinking about this from the second I woke up until the second I went to bed. During the evening when my wife and I would have dinner, I was running different runs to test different theories of how this would work and experiments and stuff like that. I think she might have a better representation of exactly what that summer was like from her point of view, but I remember it as being completely all-consuming. I need to go meet them. And so I flew to San Francisco. I showed up on a Monday afternoon and they were having problems with the Postgres database. I just tried to teach them everything that I knew about running Postgres at scale. That must have convinced them that maybe I knew a little bit about databases because they felt compelled enough that they started the migration. And of course, this company is Cursor. Notion had finally ingested their first workload. It was really slow. Justine Li found 300 milliseconds in the span of three hours that night. There was another time where Notion asked, "Can we do this, Corey?" And Justine just told them in Slack, "Yes, we can," and then worked like I've never seen before for the next 24 hours to get it in their hands. When people talk about forcing something or willing something into existence, this is one of the first things that I think of.

**Pablo Srugo** [1:27]:

Simon, welcome to the show. Dude, there are very few people I interview on this show that happen to also be from Ottawa, but you're close by, which is nice to see.

**Simon Eskildsen** [1:34]:

Yeah, I'm not from here. I moved here back in the day for Shopify. That's how I ended up in the new world. But it's actually an underrated place to build a company from. It has its advantages and disadvantages.

**Pablo Srugo** [1:45]:

Let's get into all this. You're building turbopuffer. You've got some massive logos, right? Cursor, Notion, Linear—like massive, massive companies using your product. And you've done it all bootstrapped, purposefully so, intentionally so. You're keeping things pretty much under wraps. Let's start at the beginning. Give me just a little bit of your background. You mentioned where you moved from.

**Simon Eskildsen** [2:03]:

Yeah, I should specify that we're not bootstrapped. We have taken some funding back in early 2024. We raised from one fantastic partner around...

**Pablo Srugo** [2:11]:

Seven million or like around there? Kind of small pre-seed type amounts?

**Simon Eskildsen** [2:16]:

That would look laughable in the face of what the rounds look like today.

**Pablo Srugo** [2:20]:

Gotcha.

**Simon Eskildsen** [2:20]:

So I grew up in Denmark. I moved to Shopify to work there as a software engineer, as what I thought was going to be a gap year back in 2013. I moved over to work on the infrastructure team at Shopify. At the time, Shopify only had an office here in Ottawa, where we both are today. Of course, now they have many offices around the world.

**Pablo Srugo** [2:38]:

Yeah, 2013 was pretty small. How many people? They were pre-IPO, they were what, like Series C company or so?

**Simon Eskildsen** [2:43]:

That's right. They were around 150 people when I joined, maybe around 200 between when I interviewed and when I joined. I spent almost 10 years there building infrastructure on the infrastructure team. It was a very small team at the time and grew quite a bit as Shopify grew. We were doing a few hundred requests per second when I joined, and by the time I left we were handling millions of requests per second. The majority of what you spend your time on if you're scaling infrastructure are the databases and the database layer. That's where I spent almost all of my time, caching databases and everything.

**Simon Eskildsen** [3:16]:

And everything. The database that brought me the most trouble, both in terms of the product that we could ship and in terms of the operational experience, was the search engine at Shopify. I never thought that I was ever going to work on that again. But after Shopify, I encountered the problem again of working for a friend's company, an actual bootstrap company. I worked there for a few months just helping them with infrastructure challenges and doing a short consulting stint. They asked me to build a recommendation engine. So I built a small recommendation engine and used some vectors because, I mean, vectors are great, right? They're a very good representation of what content is.

**Pablo Srugo** [3:51]:

Yeah, maybe give like the non-technical kind of explanation of where this comes into play.

**Simon Eskildsen** [3:55]:

The way I think about a vector is that if you take an LLM like ChatGPT and others and you sort of cut off their head, then what you get is you get a bunch of numbers, maybe 4,000 numbers. What these numbers are really just a coordinate in a coordinate system. You can think about this in two dimensions where if you are Spotify, you plot all of the songs that you have into two dimensions, and the ones that are similar are going to be adjacent. This means that you end up with clusters, right? You're going to have a rock cluster, you're going to have a pop cluster, whatever. You zoom in, you're going to get all those sub-genres. You can train a model that is very good at plotting unstructured data like text, songs, images, video, whatever. Some people call this multimodal, and what it really just is is training a model to put content that is similar adjacent in the coordinate system. The example I often use is that in your Shopify store, for example, if you search for a red dress and they only have a burgundy skirt, well, that's actually a very difficult search problem. It's devilishly PhD-level difficult to do in a generic way with a classical model. ChatGPT can do this with ease, right? It can see that those things are very similar in the coordinate system; they'd be right next to each other in color. So that is what a vector is. A vector is a coordinate in a coordinate system. It turns out that we can't just plot things in two-dimensional space; there's too much information to try to compress, which is what a vector is. So we use these coordinates that are really in thousands of dimensions or at least hundreds of dimensions. The problem with these things is that they are very large, but the benefit is that they're really good at doing recommendations and doing search. It means that we can have search that is really good as almost a byproduct of training these LLMs—not quite, but it's not a terrible way to think about it because they're key functionalities. In this case, in this use case, to just know what is similar to what in some of these dimensions.

**Simon Eskildsen** [5:45]:

Exactly. I mean, it's one of the things where ChatGPT and these LLMs are best at. You ask it for definitions, you ask it for more words that are similar to this word, or you ask it for what emojis are suitable for this particular thing. They're so good at finding information that is similar to other information. That is how they're trained. Vectors are just a way to do that that is really applicable to search. What we do is take text, images, whatever, and turn it into one of these vectors. We don't do that; we don't take the data and create the coordinate; we just store the coordinate system. So if you were Spotify—Spotify is not a customer—but if you were Spotify, then you would take all those songs and put them into turbopuffer and then make searches like, "Okay, Pablo, we're going to make your Discover Weekly now," or some recommendation. Let's look at everything that Pablo has listened to and then search for vectors or songs that are similar in space. That's what a vector is. And turbopuffer is the coordinate system. You can ask it questions. The innovation of turbopuffer is that it is almost 100 times cheaper than the incumbents at the time to store that coordinate system.

**Pablo Srugo** [6:50]:

And by the way, why did you leave Shopify in the first place?

**Simon Eskildsen** [6:52]:

I left Shopify because I had been there a very long time working on infrastructure, and I wanted to see infrastructure in other places. I thought that it was time. You spend 10 years in one place, and you want to see computers in a different place.

**Pablo Srugo** [7:06]:

And you left for your friend's startup, or you left and then kind of that happened?

**Simon Eskildsen** [7:10]:

I left to get in the best shape of my life at first over that summer, and then I started working with my friends' companies. I called it angel engineering, where I went for a couple of months to my friends' companies, vested mainly equity, and helped them with some infrastructure challenges. I did some of that before I did this particular one, but this was the one that was part of the founding journey of turbopuffer, where we mostly did database scalability stuff. For this one, what I did was build this recommendation engine. This was a real bootstrap company that was spending $3,000 or so per month on their Postgres, and I ran the back of the napkin math that this recommendation engine built the coordinates, right? You put these articles in a space and you recommend other articles.

**Pablo Srugo** [7:48]:

What was the use case? What did they want recommendations for?

**Simon Eskildsen** [7:52]:

Have you heard about this company called Readwise?

**Pablo Srugo** [7:55]:

Yeah, yeah, that sounds familiar.

**Simon Eskildsen** [7:58]:

Their first product was you take your Kindle highlights, you send an email every day on taking some of your Kindle highlights and showing them back to you and helping you retain your reading better. But now they have a reader product where you can save articles, you can save books, you can save PDFs, and then read them on your phone in a nice standard web interface. You can imagine in products like that, you might want recommendations. I think they still don't have recommendations, but this was the founding use case. So I built a very small recommendation engine, and it worked okay for something very simple. It actually worked okay. What I learned, though, was that I did the napkin math on putting this in production. They have hundreds of millions of articles, and it would have cost about $30,000 a month to put it in production on one of the incumbents at the time. That was just way too much, right? They could not earn a return on that kind of cost. It just didn't make sense for the per-user cost based on what they were charging. So we tabled it. It was going to work, but we tabled it because the cost wasn't there. I just couldn't stop thinking about that. It's like, why? The token cost will probably come down. They haven't really come down, but eventually, you know, they'll come down. Who was going to do that for search? Because here was one use case in one company. There's got to be thousands, if not tens of thousands of these.

**Pablo Srugo** [9:06]:

Was there something that told you that it should be cheaper, like much cheaper than what it was?

**Simon Eskildsen** [9:11]:

The fact that they didn't ship a feature because the infrastructure just fundamentally wasn't at a price point where they could earn a return on what the infrastructure provider was charging them. Not because the infrastructure provider was bad, just because that's how they implemented it. Just made me, you know, I have this GitHub repo called Napkin Math, and in Napkin Math, I have a list of numbers. How many gigabytes per second can you read from a disk? How much can you read over a network? What's the latency from Sydney to New York? All these kinds of numbers. The way that I approached the problem was, okay, well, is there a fundamental compromise that you could make here where you give something up and you get the cost in return? It turned out to me, as I stared enough at the numbers and drew this enough time, that there might be a way now that you could build a database that you wouldn't have been able to build 10 years ago that will make this really cost-efficient. Have you heard of S3? Do you know what it is?

**Pablo Srugo** [10:06]:

Like an S3 bucket, like AWS stuff?

**Simon Eskildsen** [10:09]:

Yeah.

**Pablo Srugo** [10:09]:

Yeah.

**Simon Eskildsen** [10:09]:

So for the readers who don't know what it is, S3 is this incredibly cheap way to store data. If you put data in memory, it costs maybe $2 to $5 per gigabyte of data you put in memory, right? And if you put it in an S3 bucket, the data costs about two cents per gigabyte.

**Pablo Srugo** [10:25]:

Wow.

**Simon Eskildsen** [10:26]:

Like a true two orders of magnitude cheaper. But no one had really built a database that would be fast enough for recommendations and for these types of restoring the coordinate system on top of S3. The fundamental thing that you give up is to write into S3 takes hundreds of milliseconds. That's not acceptable if you're building a checkout system at Shopify or you are browsing the Netflix catalog or some other consumer use case. It is not an acceptable latency to write new data.

**Pablo Srugo** [10:57]:

Because that comes up when, like, in that use case where you're checking out and now you have to wait 200 milliseconds for the checkout to go through or whatever.

**Simon Eskildsen** [11:03]:

When you do a checkout, you're doing much more than one write to the database. You're potentially doing hundreds, right? So you multiply 200 milliseconds by 100, and you end up in tens of seconds. A normal database can do a write in hundreds of microseconds, right?

**Pablo Srugo** [11:14]:

Three orders of magnitude faster than you can do it to S3.

**Simon Eskildsen** [11:15]:

So, okay, you might give up three orders of magnitude in write latency, but actually you don't have to give anything else up. And then you have this true two orders of magnitude decrease in the base cost of the system. That's a great compromise if you're building search because when you're building search, you are okay with, you know, if you add a new title to the Netflix catalog or you're adding a new song to Spotify or even if you're adding a new product to Shopify, it's okay that it takes 200 milliseconds to make it into the search engine. It's an acceptable latency.

**Pablo Srugo** [11:49]:

It doesn't really matter because the thing about search is when you search, you want to be fast, but when you add something to that database search, no big deal.

**Simon Eskildsen** [11:55]:

Exactly. So we could build that database. S3 and disks and a bunch of other things that are kind of boring and nerdy sort of meant that we couldn't have built this database 10 years ago, but now we can build it with this particular architecture. And then if you query it a lot, right, we just put it into memory, right? If the data is active and pay a little bit more for it. That's the fundamental architecture that I came up with in 2023, and then sort of locked myself in and came out with the first version of the database over the summer of 2023.

**Pablo Srugo** [12:25]:

How quick was this, by the way? From the time where you hit this, "Okay, this is not going to be affordable for Readwise," until you have the epiphany, like just walk me through that timeline, the epiphany, and then you lock yourself up.

**Simon Eskildsen** [12:35]:

So I worked with Readwise on this in around September-October of 2022, right? So this is around when ChatGPT was coming out. The problem bothered me for the next six months, and I was working with some other companies during that time.

**Pablo Srugo** [12:47]:

Right.

**Simon Eskildsen** [12:47]:

And then finally, in May of 2023, a consulting agreement fell through. It's just like it was not going to work out. I was talking to a friend and I was explaining to him. I was like, "I think you could build a search engine with these compromises." God bless this guy; he's one of my best friends. He actually helped me design the website. Well, he designed the website; I mostly just provided really annoying feedback on the website, and I think he would agree that he did not understand what I was going on about. Hopefully, I'm able to articulate it better now than I was then. He just said, "You should just do it."

**Pablo Srugo** [13:20]:

He just saw your passion. I mean, he knew you well enough to be like, "Dude."

**Simon Eskildsen** [13:23]:

Yeah, I was like, this guy's kind of being annoying for this consulting agreement, and I don't know if I want to do it, but I have this other idea, and it's percolating in my head. It's like, "You should do it." I said, "I think we could do this cool thing with a website where it feels askew and sort of retro, and I think there's something there, but not take it too far because then it's going to feel in the uncanny valley." We're still riffing on that, and he's like, "You need a name." I was like, "Well, how are we going to come up with a name?" He said, "You should choose a name that has an emoji that doesn't have any other connotations." We were looking at our phones on a couch in Airbnb in Copenhagen, and we find the puffer fish emoji. We both agree that, okay, well, no one knows what this emoji means, or it doesn't have any other connotations. Then we're sort of throwing it around. I think our wives wake up around this time, and his wife says, "turbopuffer," and she said it in an Australian accent, which is, by the way, that and the French accent are the superior accents to say the name in. I'm not going to try to...

**Pablo Srugo** [14:19]:

You're not going to try? Come on, man.

**Simon Eskildsen** [14:20]:

Not going to try. Not going to try.

**Pablo Srugo** [14:22]:

Right.

**Simon Eskildsen** [14:24]:

Something like that. And it just made us happy. That's really what it is. To date, the way that we explain it, right, is that you put the data on S3, and it's slow and cheap. And then when you query the data, it gets faster and faster as we sort of puff it up into RAM, right, and disk as an intermediate layer. But that is the origin story. So, you know, September of 2022, October, I'm working on this recommender system at Readwise and various things to integrate the first GPT models that were really good into that experience. Then I did another consulting agreement with another company called Replicate, and then I went and started building this in May of 2023 is when the great lock-in happened.

**Pablo Srugo** [15:01]:

Did you have a sense of just how big the market was, or was this just like, "This needs to exist, I'm going to build it, we'll see kind of what comes of it?" Do you know what I mean? Like how much did you size things out in terms of the opportunity in front of you?

**Simon Eskildsen** [15:10]:

I didn't do any of that. I can articulate why now I did it, but at the time, it was your personal neural nets, gradients are trained on your experience. What I just saw was that there are these vectors; they are enormous, right? So what we didn't talk about with these coordinates is that because they're in thousands of dimensions, the coordinate in the coordinate system is very counterintuitive. But the coordinate in the coordinate system is larger than the original data. If you take a paragraph of text, the coordinate that represents it is actually larger than the paragraph of text. It is just so much data; it's so large that the costs are so great that it's very difficult for most companies to earn a return on. That was the fundamental first insight: this is really big, and it's just too expensive. The second thing was that S3 buckets and disks and a bunch of other things—there are some technological advancements that meant that we could build a database in a way we couldn't build before. The way that I articulate this now is a little bit more structured. If you want to build a generational database company, you need two and a half things. The first thing that you need is that you need a new workload because if you want to make it as a really large billion-dollar-plus database company, you need a reason for more or less every company in the world to need a new database or at least use new products that use your database. That new workload is that we have AI that wants to be connected to enormous amounts of data. We are the latter part of that, right? We are the coordinate system, right, that AI searches in. When you train an AI model, you're trying to basically compress all of human knowledge into a few terabytes. That's impossible. You can't do that. And these, not by any computer science or any science really, does not make sense that you can compress all of human knowledge into a few terabytes. What you can compress into a few terabytes is a way to reason about the world and a mental model of the world. But in order for you to use that, you have to have access to tools, and the tool that we are is search over all of the unstructured data potentially in the world inside of every company in the world. That's the new workload: connected data to AI. That's the first thing you need to build a very large database company.

**Pablo Srugo** [17:26]:

How much was this front and center at that time that this would be the new workload for you?

**Simon Eskildsen** [17:30]:

The new workload was completely overfit on Readwise, right? It was like, "Well, Readwise has this new workload; maybe other companies do," right? It was completely overfit on that one customer. So I assume that latent demand is when there is demand in the market that is pent up, and you can't access it because of pricing or availability, or there's some forces at play that mean that you can't get what you want. The reason why it was wanted was a search engine that was about one to two orders of magnitude cheaper because then it would have been a no-brainer to search it. So I felt, well, there's got to be a hundred other companies. This feels worth at least writing an article about, right? It's the output of the summer, and I was in a good position where I was looking for a project, and the project found that was a new workload. The second thing you need to do to build a really large database company is that you need a fundamentally new way to store the data. That's what we just talked about: put it on S3, two orders of magnitude cheaper in RAM, and then the stuff that you actually need to put in RAM, you put into RAM when needed, but you only puff it up when you need it to actually retrieve the data. That's the new storage architecture. None of the existing databases have that storage architecture because otherwise, there's no reason why the other big database companies like Snowflake and Databricks and MongoDB and Oracle would not just get good at it unless it fundamentally challenges assumptions that they've built on top of and relied on for decades. Really quickly, you don't strictly need this, but if you're impatient, then you want this because otherwise, you need to wait a long time for the compounding function to work on a smaller base. That's what you need. I could not have articulated at the time, but it felt like that was in the air, right? The way that we think about innovation is that, I mean, it's more of a discovery than it is about doing anything genius. The discovery here was that there's a set of compromises we can make, and it's uniquely suitable for this new workload: slower writes and no other real compromise.

**Pablo Srugo** [19:23]:

The other framework for that, the innovation piece, is the Mike Maples thing, right? About kind of living in the future and so this idea that you're kind of out there doing things. You're not actively trying to start a company or trying to find a problem; you're just doing things as you were doing things, and then you came across this search situation, and you just went out and solved it. Then AI happened to be this wave that has made the opportunity so much bigger. It's just crazy how things work out.

**Simon Eskildsen** [19:46]:

That's right. So that summer, it was never a business. It was a project. It was a project where I thought that I was maybe publishing a really interesting article, and I actually did, and I wrote an article about the experiments that I'd done, and they sort of, like, a few months or two into it, and I sent it to a friend, and he said, "You should turn this into a real service." That was enough encouragement for me because he was one of the, you know, industry leaders in this space of practitioners. So I just kept working at it, and there were some real problems to solve, surely, but it was not something I felt like I know a lot about databases and so on, but it was nothing that was like uniquely really complicated, right?

**Pablo Srugo** [20:25]:

And it was just you.

**Simon Eskildsen** [20:26]:

It's just me. Inside of the company, you know, we make a lot of fun of what we call v1. There's almost none of v1 left now other than some pieces, and we all make fun of it. My joke is that they make fun of it, but it's the reason they have a job now. This doesn't really resonate because these people are so good that it would not be an issue for them to get another job. I was obsessed that summer. It's all I did, and we would have guests, and I couldn't stop thinking about it. I was just in a notebook. My wife was very patient but also a little bit annoyed, rightly so.

**Pablo Srugo** [20:57]:

How all in? What are we talking about? What would a normal week be or day be for you in those two or three months where you're building?

**Simon Eskildsen** [21:04]:

I was thinking about this from the second I woke up until the second I went to bed. During the evening when my wife and I would have dinner, I was running different runs to test different theories of how this would work and experiments and stuff like that. I think she might have a better representation of exactly what that summer was like from her point of view. But I remember it as being completely all-consuming. I remember not being as present as I wanted to be when we had guests and we were hanging out with people because I couldn't stop thinking about it. I was truly engrossed. I felt that if I didn't do it, someone else was going to do it. Every day, the opportunity felt larger and larger. I did three rewrites during that time. I tested every single way that I could think of to do it to make sure that what I did was the best. I changed platforms. I tried so many different things, and finally, it was the fourth of October, two years ago from when we're recording, I finally released it on X.

**Pablo Srugo** [21:53]:

Did you have a following? Let's start there. What's the context of you on X? Are you big? You're posting a lot? You've got people that follow you, technical people?

**Simon Eskildsen** [22:00]:

I just have an egg avatar and 200 followers, no.

**Pablo Srugo** [22:02]:

There you go.

**Simon Eskildsen** [22:02]:

Yeah, no, because of my articles on Napkin Math, and when I was a child, I did a lot of talks at conferences about scaling databases and things like that. I had accumulated a small following on X, and so it got really good traction on X. People were encouraging, and it was enough for me to feel that maybe I have something here.

**Pablo Srugo** [22:20]:

Do you remember what, like, what we've done? Like a thousand, ten thousand views, hundred thousand views? What kind of order of magnitude of attention do you get?

**Simon Eskildsen** [22:27]:

I can pull up the tweet right here because I was just reposting it the other day. Post engagement: almost 400,000 views.

**Pablo Srugo** [22:34]:

Okay, yeah, so it was pretty big.

**Simon Eskildsen** [22:36]:

The website was really simple, great encouragement, and I hadn't shared with anyone that I was working on this because the problem is when you talk about what you're doing, talking about it feels almost as good as actually doing the thing. So you got to just do the thing. I launched it, and I got a really fun email. I got a fun email from a company in San Francisco, a small company in San Francisco. They were just six people, and they more or less said that, "Hey, we're working with this peer of ours, and the trade-offs that they're making and the economics just don't make sense." They were spending six figures on this, and the fundamental problem they had was that the per-user cost was, let's say, a dollar per user. I don't know what it actually was, right? But like something that was untenable, and maybe they were charging them twenty dollars, maybe it was five dollars per user. It was growing at a trend where the per-dollar cost was not something that they could earn a return on.

**Pablo Srugo** [23:27]:

Very similar to your Readwise original kind of issue.

**Simon Eskildsen** [23:30]:

And Readwise wasn't even the first customer there. They only became a customer actually a year later, which is very funny. I think that if I hadn't gotten so busy, I would have just gone inside Readwise and then shipped the thing right on turbopuffer. But we got really busy really quickly. So I got this email: "Hey, we're spending six figures; the economics don't work for us; we're very interested in what we're building." Knowing that team now, I know that they would have sat around the dining table and said, "Why hasn't anyone just put all this on S3? When you query it, you put it into RAM, and it's as fast as anyone else, and you just pay that little penalty of at the very beginning when you start interacting, it's a little slow, and then it gets fast." That's the right index, exactly the compromise to be made. For whatever reason, I had the conviction at the time to do two things. The first thing I did was that I texted who I thought was the best engineer that I worked with during my time at Shopify, Justine, and I asked Justine, "Do you want to come on? I can't pay you anything, but maybe we can work something out. This is what I'm working on." I sent her a bullet point list of the things that she would work on, and Justine said, "Let's take a walk." We took a walk, and Justine started working on it in the beginning a little part-time. Three or four days in, she said, "I want to be all in." Then Justine came on as co-founder. The other thing that happened around that time is that for whatever reason, I had the conviction with this small company in San Francisco that I'd never heard of—I’ll reveal the name in a moment—that I should fly to San Francisco and meet these guys. And so I did. I flew to San Francisco.

**Pablo Srugo** [24:54]:

Who were they?

**Simon Eskildsen** [24:54]:

They were impossible to track down. They were so busy that the only call that I could get with them was that at one point one of the co-founders emailed me at 5:20 a.m. on a Thursday and said, "Hey, can you meet now? I'm free," after missing two meetings because of outages.

**Pablo Srugo** [25:11]:

5:20 Ottawa time, by the way. Which time?

**Simon Eskildsen** [25:13]:

Yeah, so for him, it would have been 2:20 a.m., right?

**Pablo Srugo** [25:17]:

Right.

**Simon Eskildsen** [25:17]:

I happened to be up, and so I was like, "Yeah, we can jump on a call." We hopped on a call and explained all of it. After that call, I felt okay, like we have this email exchange. I need to go meet them because it's too hard to get them on a call. I just need to go see them in person. So I flew to San Francisco, and I didn't tell them that I was in town for them. I said, "I'm in San Francisco on Monday; let me drive out, drop by the office." No problem. I showed up on a Monday afternoon, like basically the day that I got there.

**Pablo Srugo** [25:42]:

Sure.

**Simon Eskildsen** [25:42]:

When I showed up at the office—and this is how I remember the story; I need to actually confirm this with the team how they remember, but this is how I remember it—but you can't trust your memories on these things. The way I remember it is that I showed up, and they were having problems with the Postgres database. Maybe it was down; there was some issue with it, and they got it back up. I spent a lot of time with Postgres, and so I just tried to teach them everything that I knew about running Postgres at scale, installing these different things, just helping them as much as I could. That must have convinced them that maybe I knew a little bit about databases because then when I told them what it was going to take and all of that, they felt compelled enough that they started the migration.

**Pablo Srugo** [26:17]:

Which is a massive deal, to be clear, just in terms of trust. It's not like trying out a normal product.

**Simon Eskildsen** [26:23]:

Of course, this company is Cursor.

**Pablo Srugo** [26:26]:

We have tens of thousands of people who have followed the show. Are you one of those people? You want to be a part of the group. You want to be a part of those tens of thousands of followers. So hit the follow button.

**Simon Eskildsen** [26:36]:

A little company, four founders. There were six people then, and I don't know how much revenue they were at, right? This is in October or November of 2023. They needed two features, and Justine and I worked very hard to get them those two features. They started onboarding; they had a few billion vectors, which at the time was one of the largest workloads in the world, and they just did it over the course of a week. They migrated everything that they had, and their first bill was 95 percent smaller than their six-figure bill that they had before, right? So they went from a six-figure bill to a four-figure bill. Of course, Cursor has grown a lot since, but it started a very special relationship. In the beginning, I was so honored that they called me once sometimes and asked me to help them with infrastructure candidates to explain the opportunity of Cursor. I was probably one of the few people looking at our dashboard that could see their growth, and I sold them as hard as I could to the few candidates that came through.

**Pablo Srugo** [27:33]:

I'm sure they're very happy now.

**Simon Eskildsen** [27:35]:

I don't know why they needed me for this, frankly, because they are so good at recruitment that I always felt honored when they asked and happy to do it. They even asked me for infrastructure advice and how to lead teams and things like that. I think they're probably much better than me at this by now, but they were humble, and they asked for advice where they could, and it forged a very special relationship at the time—one that I think continues to this day. We were in Slack, and these are not their words, but we truly felt like we were part of the Cursor team, and we had our little function, and we just tried to make sure because my goal was like one of the things that almost brought me to tears at some point was just Sualeh, one of the co-founders, just saying, sort of, "This is one of the only things that we just don't worry about." We have not had to worry about it as we scaled up. As someone who has been on call for a pager at Shopify that lost millions a minute, it's humbling for us, and it's an absolute honor and privilege when one of the fastest-growing businesses of all time by revenue tells you that you are one of the pieces of infrastructure that they've had to worry about the least. So we leaned in heavily. Our only customer to begin with, and we built everything to make sure that it worked for them, and they used every single line of code in the product. Justine and I just worked very hard to make sure that this was going to work.

**Pablo Srugo** [28:46]:

Just the two of you kind of at that point. How quickly did that Cursor take you to a million ARR?

**Simon Eskildsen** [28:50]:

Not just Cursor alone, right? We reached a million in ARR about a year after we launched, but we had more customers then than just Cursor. We also had Notion and a bunch of other customers then.

**Pablo Srugo** [29:01]:

We'll get into how you landed those others, but just on Cursor, I'm curious. Going back to that Readwise thing, because with Readwise and these other examples, it was at the point where without turbopuffer, like, they just couldn't launch the feature, right? You made it way cheaper for them. If you hadn't come along, could they even have scaled to what they scaled to without either turbopuffer or something like turbopuffer to make the numbers work?

**Simon Eskildsen** [29:21]:

I think that if turbopuffer hadn't come along, they would have built it themselves in-house.

**Pablo Srugo** [29:26]:

They would have needed to have that sort of decrease in cost to make everything else make sense.

**Simon Eskildsen** [29:31]:

They would have built turbopuffer themselves, and I think they would have done a great job, but it would have meant that they would have had engineers that were solely dedicated on that—really good engineers who could be working on making Cursor better. It was a great partnership, right? I think it was very mutually beneficial. But now there are many companies that have launched turbopuffer, right? Again, it was a discovery. It was not that it's like fundamentally the hardest thing to do, but of course, you need a lot of good engineering for a long period of time to make sure the product is really good. But Cursor could have built it for sure. They have one of the best engineering teams in the world.

**Pablo Srugo** [30:01]:

I mean, to be fair, that goes for many products. Many products could, in theory, be built, but most people want to focus on their own product, and especially the infrastructure, you know, kind of use others. So you and Justine, you're working on Cursor just for how long?

**Simon Eskildsen** [30:12]:

You could sign up in December, so we had a bunch of other smaller companies. Believe it or not, the second larger deal in the thousands of dollars per month was Telus. For those of you who don't know, Telus is one of the largest, if not the largest, telco in Canada. They were very much what you would consider an enterprise company, but someone inside of Telus, Justin Watts, just saw turbopuffer and similar. He'd had all these problems with vector search and search in general with these other providers, and he saw something in us. To this day, I give Justin Watts so much credit. It's very rare that an enterprise gets that amount of leeway to bet on a startup.

**Pablo Srugo** [30:53]:

At the database layer too, yeah, no less.

**Simon Eskildsen** [30:56]:

We spun up a Canadian region for them within a few hours after them asking, and we were just like, "Look, we're going to work very hard for you. We know that you're putting yourself a little bit on the line here. Tell us what you're paying, and I think we can do a lot better than that." We just had a frank conversation with him, and they're very happy customers still.

**Pablo Srugo** [31:11]:

What do they power, by the way? What's the use case for them?

**Simon Eskildsen** [31:13]:

They have a bunch of internal systems, but they have this platform that they work with their customers on integrating called Fuel iX, where they use turbopuffer to search. Then they have a pluggable model layer, and then they sell that to their customers. I think they use it for a bunch of things internally. They call them internal co-pilots, and they use it for a bunch of different things.

**Pablo Srugo** [31:31]:

So it's AI use cases, but a lot of internal.

**Simon Eskildsen** [31:34]:

Yeah, inside of Telus, they were our second larger customer.

**Pablo Srugo** [31:37]:

In December, you said, like two months after?

**Simon Eskildsen** [31:40]:

This was in maybe February or March.

**Pablo Srugo** [31:42]:

So, I mean, four or five months.

**Simon Eskildsen** [31:43]:

And then there was a bunch of random small startups. There's this company called Merlin, a Chrome extension. There was a company called Fixie, and they wrote an article. It was like an ex-Shopify founder; now they're doing voice agents. They built this, like, you know when you watch a sports roster where they trial all these databases against each other, and that ended with turbopuffer winning? That article was really generous for us. It was a bunch of startups like that, like a dozen or so back then. I mean, Cursor was not that big. I didn't even know them back then, right? In the beginning of 2024, they just launched in May of 2023, I believe it was, so they were not that big in October 2023.

**Pablo Srugo** [32:23]:

How did Telus, how did these other startups find you? Is it through X? Is it just like word of mouth? Or are you doing anything actively to get customers?

**Simon Eskildsen** [32:31]:

It was just X. That's just how they found it. We didn't have any other channel for...

**Pablo Srugo** [32:35]:

Did you keep posting, though, on X? You were doing that pretty consistently?

**Simon Eskildsen** [32:38]:

Sure, yeah. I was like, "I'll just improve performance by 20 percent," you know, things like that. Yeah, for sure. I was very active on X. Justine was active on X, and the turbopuffer account was active. I think one of the things early on that also caught people's attention is that the website looks very similar now to what it looked like back then, but it had a very particular brand and flair. Me and Joachim, who is the designer that helped me come up with the name and the initial website, we spent so many hours on that, right? Joachim was basically the first outside person other than Justine and I on turbopuffer, just working on the design. That design got a lot of attention back then, and now I think it's—I don't really know. We didn't have any inspiration that was super close to it, but we just spent an enormous amount of time on it. Now I would probably look at it and be like, "How? Why did we spend hundreds of hours on this?"

**Pablo Srugo** [33:26]:

That was giving me my question. Why did you—you can go both ways on websites. There are people who think it matters so much. Other people could say, "Who cares? If your product's great, your product is great." You could take both sides of that debate and I'm sure find examples for both.

**Simon Eskildsen** [33:41]:

During my time at Shopify, I have looked at if not hundreds of database websites, right? Again, what you care about with a database is just what does it do? What are the trade-offs? What are the guarantees? What are the limits? Who's using it? What does it cost? That's really all you care about. I just wanted all those questions answered on the homepage. I wanted an aesthetic that said we care about design and UX, and you're going to have a great time. We've spent some time to make sure that everything is organized in a way that makes sense, but not so much that you're not sure if you're buying a database or a sneaker. So that's really what I wanted with the website. Then we have this particular monospaced type of aesthetic that we've iterated on in many, many iterations since then. It just felt to me that it was so important to position as we care about your experience. We care about DevX. We are hardcore database engineers, and we want to make it very clear for you to understand whether this is the right database for you. And if not, that's cool.

**Pablo Srugo** [34:40]:

I mean, you literally had—I don't know if this started, but you have like a—you can toggle around depending on your workload, see exactly how much it's going to cost you.

**Simon Eskildsen** [34:47]:

That's right. Yeah.

**Pablo Srugo** [34:48]:

So, Telus, through X. Now we're in 2024, right? This is like when you realize that Cursor is like, "Holy shit, good." You know, like these guys are growing at an insane, insane rate, and they're going to be a huge customer for you guys.

**Simon Eskildsen** [35:02]:

I think we were just always wondering, like, when are they going to stop growing? Compounding is a very weird thing where it's very slow until it's incredibly fast. It was not clear to us. We were so focused on Notion because at the time, right, they sort of started POCing with us in the spring of 2024, and we were so focused on them.

**Pablo Srugo** [35:22]:

Also came inbound, like same thing through X?

**Simon Eskildsen** [35:24]:

No, outbound.

**Pablo Srugo** [35:25]:

Yeah, walk me through that. What were you doing on the event side?

**Simon Eskildsen** [35:28]:

I mean, maybe we can—the other things that were happening around then at the beginning was that we were a bunch of VCs got interested. Generally, these were the VCs who were very technical, who also saw, but maybe also couldn't quite articulate that there's something here. We ended up choosing this investor called Lachy, and the reason that I loved Lachy was for three reasons. The first reason was that I met him on the first trip that I went to San Francisco. I was actually introduced by the Readwise co-founder. He said, "Just go talk to this guy." I went on a walk with him, and he told me on the walk, "Simon, if you're serious about this company, you need to get this lawyer because he's the guy that always leaves me basically crawling out of the room." It's such an odd recommendation for a VC, right? I'm like, "That's incredible." Because, yeah, if you are serious about your company, you need one of the best lawyers in the world. It's completely underrated. This lawyer team that we work with at Wilson Sonsini, they take a small bit of equity, so they're completely aligned with you—common stock. They just see more transactions than anyone else, and people underestimate how much this matters. Well, then I talked to one of the funds, and the fund was like, "Do you have a lawyer?" Right? Because a good sign whether a startup is serious is, and I said, "Oh yeah, this guy Rob Broderick and Damien Weiss at Wilson Sonsini." He said, "Oh no, no, no, no. Don't talk to those guys. Here's my guy." That's how I knew that this is a good lawyer.

**Pablo Srugo** [36:44]:

And they’ve been helpful. I'm curious on this: where did they have the most value for you?

**Simon Eskildsen** [36:49]:

I think the concept of having lawyers for different things before being a founder was a very nebulous concept to me. But a corporate lawyer is essentially the person who cares about stock option grants, like your cap table, anything that revolves basically exchanging equity for cash, right? Or for more equity or whatever, right? So these people become very important on partnerships. They become important for you. Then you want people who are your coach in your corner. If you are raising money, then you, of course, need someone who sees a lot of that. If you are thinking about your corporate structure and like what should you have in the U.S. versus Canada, you need really good corporate lawyers. We have corporate lawyers in Canada and in the U.S. But you want a corporate lawyer who sees a lot of activity and ideally a corporate lawyer who never works with VCs but only ever works with founders. Ideally, your corporate lawyer is available on WhatsApp within five minutes at all times for any questions that you might have. That's what these lawyers are, and their incentive is aligned with you on anything that might happen, whether you're IPO-ing, whether you're selling your company, or whether you're doing investment because they have the same stock that's diluted exactly the same as you and all of the employees. I talked to other lawyers at the beginning, and when they told me they're hourly, you can just see their daddy issues come out through their eyes. That is not what you want.

**Pablo Srugo** [38:09]:

So this VC makes that recommendation. When do you go back to him and decide that you want to raise?

**Simon Eskildsen** [38:14]:

Yeah, so the other thing he did was that he was just helpful immediately, right? I got to taste before I bought, and he was just incredibly responsive. Then there was another thing, which was that he didn't know anything about databases, and he was honest about it. He just said, "Yeah, I said, have you ever invested in a database company?" "No, I've never invested in a database company." I just said it with such conviction that I was like, "Oh, maybe that doesn't matter. Maybe it's actually a good thing because Justine and I should know about databases. The people that we hire should know about databases. It doesn't really matter if the investor knows about databases other than they might find us earlier." He was just incredibly well-connected, so yeah, those were the reasons that went into choosing him.

**Pablo Srugo** [38:48]:

When you went and you decided to raise some small pre-seed that you won't see exactly how much, why did you need that money for? I assume you want to hire, like how many people do you want to hire with that?

**Simon Eskildsen** [38:56]:

Yeah, so in the beginning, we were like everything just ran on Justine's and my credit cards. We were trying as hard as we could to not lose money. I mean, we didn't know how to raise money; we didn't know how all this stuff worked, so we were just trying to not lose money. We were optimizing the Cursor workload like crazy to just not lose money. When I was in high school, I went to this competition called IOI. Have you ever heard of this?

**Pablo Srugo** [39:15]:

No.

**Simon Eskildsen** [39:16]:

So...

**Pablo Srugo** [39:16]:

I haven't.

**Simon Eskildsen** [39:16]:

IOI is an international competition where like 90 to 100 countries send their four students. It's basically like computer science Olympics in high school. The problem is that no one's heard about it, but it contributes more to GDP than the real Olympics. Let me give you an example: the founder of Cognition is a gold medalist. The founder of this company called ModoLabs is a gold medalist. A lot of Silicon Valley and the founders of this generation are people who did really well in these competitions. I did not win a medal, but Boyan did, and Boyan and I met in 2012 in Italy, where we both went to the competition. He was on the North Macedonian team; I was on the Danish team, and we got along. The Macedonian team called him God because he was very good at the competition, and he was a silver medalist. We got reintroduced by a friend, and he was the first employee at turbopuffer, but we didn't have the money to pay him, and we'd already paid enough money out of our own credit cards—Justine and I. We needed to have someone on board, and that was the reason why we raised.

**Pablo Srugo** [40:11]:

So the other thing I want to talk about, the outbound piece. So you said you actually got Notion through outbound. What outbound motion did you start running?

**Simon Eskildsen** [40:17]:

I just knew that Notion was such a good fit for our architecture, and we got introduced. I forget who exactly introduced us the first time, but we got reintroduced many times. Most people just ignored us to begin with, and then eventually someone took a call, and then nothing happened for like five months. So it's hard to attribute because Lachy is also an investor in Notion. So, you know, I don't know what exactly he worked, but at some point they just reached out and said, "Hey, the economics are holding us back a little bit on product, and so we want to try some other suggestions." Later, I learned I was just at dinner last week with one of the people at Notion that worked with turbopuffer. Apparently, this person and a colleague on their team, whom we now work closely with, had sketched out what they thought that the right architecture for Notion was for search, and they had basically come up with turbopuffer and then sort of matched that. I don't know exactly how we ended up on this call, but we had a call, and there were six people in the room, and it was just me on the call on our side, and they just berated us with questions about, "Can you do this? Can you do that?" Now I understand that they had already had the product vision that they're executing on today—Notion AI, you know, it's like the Cursor for pros—and they needed the right search engine to power all that with the right economics that they could earn a return on. So they asked for a region in Oregon; we spun up a region in Oregon within a day, and they just started testing. Justine pulled every trick doing this POC that—I mean, Justine and I worked together for almost eight years at Shopify, and Justine was always one of the best engineers I'd ever worked with. What I saw Justine pull to make sure that Notion was going to work was beyond anything I'd ever seen before. I remember distinctly Morgan, our second employee, had joined at the time, and we were doing a little team offsite in Quebec, Canada. We came back from dinner, and Notion had finally ingested their first workload, and it was really slow. It was like 500 milliseconds for a query, and we wanted less than 100. Justine found 300 milliseconds or something like that in the span of three hours that night, just kept banging out PRs from looking at the workload. There was another time where Notion asked, "Can we do this, Corey?" And Justine just told them in Slack, "Yes, we can. We just need to expose it in the API," and then worked like I've never seen before for the next 24 hours to get it in their hands. This is how Justine worked on this deal. When people talk about forcing something or willing something into existence, this is one of the first things that I think of. It was weeks and weeks of that POC in different angles, right? I was working on the pricing because we didn't even know how to charge for this at the time. Boyan was working on a complete rewrite of turbopuffer on the side, and Morgan was working on full-text search. We were just so focused, and finally, towards the summer, we got the feeling that maybe they were going to commit. We felt a little like it was slow, and they had some commitments with the other vendor that they were riding out. On the 25th of July, it was getting really close. My wife was seven or eight days overdue with our first child.

**Pablo Srugo** [43:06]:

Wow.

**Simon Eskildsen** [43:06]:

I was working so deeply on the billing system because otherwise, we were going to lose money on Notion because we needed to put this new billing in place. Finally, they signed on the 25th of July in 2024. I invited Justine and our virtual CFO Mike, who was now on full-time. We were the only ones in Ottawa, and then we had Morgan and Boyan on a video call, and we were all just like, you know, awkwardly on a call. My wife was there as well, very pregnant, and Justine Li says to my wife, "Wouldn't it be funny if your daughter was born today?" This is the biggest jinx of all time because this was, you know, late afternoon. They all leave, Justine and I hang out for an hour. I cried a little bit that day. It was just like we'd worked so hard on this, and Justine in particular. Justine left, and then my wife came down and said, "I think something is happening."

**Pablo Srugo** [43:54]:

No way.

**Simon Eskildsen** [43:55]:

Three or four hours later, our daughter was born—first daughter, first daughter. That was a wild, wild day.

**Pablo Srugo** [44:03]:

It would be hard to craft a better story than that. Did you know at the outset, like when you're sizing these deals, like Notion obviously is going to be a massive customer, but do you know how much revenue that's going to generate exactly, or is it an estimation? Because you're not really selling like pre-seed or whatever, obviously. So how much do you know about how big it'll actually be?

**Simon Eskildsen** [44:21]:

Yeah, I mean, at the time, I was spending probably a third of my time doing engineering and a third of my time on pricing, you know, an updated pricing that was going to work for them and making sure that we could stay afloat, and then the other third of the time selling. What we did right is you sent them a form saying, "This is like the approximate size of your workload," and then we generate a quote for them. Notion came back and said, "Are you going to lose money on this?" Now I know that Notion was legitimately concerned that we were going to go out of business because it was so much more cost-effective than what they were doing. So yeah, we knew exactly what we were going to earn on it, right? We knew like the rough margin profile. You never really know until the rubber hits the road, but we knew exactly. At this time, that workload was like Anthropic and Cursor.

**Pablo Srugo** [45:02]:

This is where I'm curious to dive into this because at this point, like say you've got Telus, you've got Cursor, which you don't know how big it is, but it is, you know, it's a growing customer. You've got a bunch of little customers. Now you sign Notion. I would argue 99 percent of founders in that situation, especially these days, they're going around, they're raising like a $30 million Series A. I mean, you've got all the—like everything you need to have to go out and do that. You don't do that, and you haven't done that. Why not?

**Simon Eskildsen** [45:25]:

There are six reasons to fundraise. Reason number one was the reason that we fundraised in January of 2024: exactly how much money that we needed to prove to ourselves that we could find product market fit. If Justine and I could not find product market fit by the end of 2024, we were just going to close up shop. Justine and I were very clear about this. We even told the investors who were invited for that round in 2024 that we were going to close up shop by the end of 2024 if we had not found PMF. That was terrifying to everyone but Lachy. We raised enough money that we knew that we could fund R&D for 2024. The second reason to raise is to fund growth. It's to fund marketing and other ways that you think that you could grow the business once you have a proven way to turn dollars into more dollars and mindshare. The third reason to raise—and this is probably the most popular reason, and it's kind of what you're getting at here—it's for ego. It's also known as raising because you can or because there is momentum. It has all kinds of ramifications that you now have to live with that might put your business at risk. The fourth reason to raise is to fund liquidity for the employees, right, that have believed and that have been here for a long time. The fifth reason to raise is for trust and or publicity. You raise basically to build credibility for your business. That would not have been an illegitimate reason for a database company to raise. The sixth reason to raise is for strategic partnerships. This can either be, you know, a VC that you really think is going to make a dramatic difference for you, either because they've proven it or they have connections or whatever that you need. But it could also be institutional investors, right, where you know that if you get this customer, they get our new cap table. It's incredibly important for your business, right? If you're building software for movie studios, you might do that for Disney, right? Or something like that. Those are the six reasons to raise, and if you are raising for any other reason than those six reasons, you are raising at reason number three, which is to fund ego.

**Pablo Srugo** [47:26]:

There's a lot of reasons there that I could argue, yeah, like why not? You know, you get more credibility, you'll get more go-to-market, you know, whatever. Like all those reasons could in theory apply to you.

**Simon Eskildsen** [47:34]:

I mean, I could go through every single one of them and we can see why they didn't apply at the time.

**Pablo Srugo** [47:37]:

Or even today. I'm curious on maybe not every single one, but the ones that would be most obvious out of what you said would probably be credibility, which would in theory help you, you know, hire and things like that even faster than you already are and, you know, go-to-market. I mean, those would be the two that jump out at me as like, "Yeah, you could raise for that, you know, small dilution, why not?"

**Simon Eskildsen** [47:54]:

The growth piece, we don't have a growth motion where we can put dollars in and get more dollars and mindshare out the other way. That may be true over time, right? That's the team that's being—that's a team we're working on building right now, and then we might raise for that reason alone.

**Pablo Srugo** [48:10]:

Because it's mainly inbound still that your growth is coming from?

**Simon Eskildsen** [48:13]:

Mainly inbound is the majority of our growth, and then on the trust piece, we have found that that has not been a limiting factor. Now, we got very lucky, right? We worked so closely with Cursor, and they grew from being maybe not a particularly known logo to an amazing brand to be associated with in a very short order. So we got incredibly lucky. But there's lots of downsides to raise. I don't have a neat list for you. When we hit one of those triggers, that's when we would consider raising. We would look at the people that we work with that we like working with when the time comes.

**Pablo Srugo** [48:46]:

How many people are you on the team?

**Simon Eskildsen** [48:47]:

The team now is like, as of October 9th, we are 17 people.

**Pablo Srugo** [48:54]:

And so I assume, obviously, you're profitable.

**Simon Eskildsen** [48:56]:

If you want to build a database company, it is a long-standing endeavor. We are profitable for that reason because we want to build an enduring, large, independent business.

**Pablo Srugo** [49:06]:

Maybe my last question on fundraising: you don't need 20 or 30 more people on the product and engineering side and then be unprofitable, but not have to worry about longevity, for example. That's just not a need for you.

**Simon Eskildsen** [49:18]:

You find me 30 database engineers of the caliber of the ones that I already have, then yeah, like then we need to raise. I'm constrained in finding those people and getting them to join turbopuffer more so than I am on capital. And by the way, these engineers want equity. That's what they're most interested in.

**Pablo Srugo** [49:37]:

I mean, you start off the call—this I think was before our recording—but you're like, you know, Ottawa's a great place to hide, right? You're kind of purposefully somewhat stealth. You're buying into the stealth piece, and even on revenue sharing, you don't want to share revenue. What's the strategy behind that? What's the thinking behind that?

**Simon Eskildsen** [49:50]:

We want to build the best database. How does disclosing various things—how does... Because how do these things help me be a big database company? If something is difficult to price and it has certain externalities that are difficult for me to price, I just don't do it. What is very clear is building a world-class database team and working on the database and connecting our databases in as many companies as possible that needed—that's very clear ROI. And that's why I spent my time doing that. Of course, we need everyone to know what turbopuffer is, and that time is coming.

**Pablo Srugo** [50:23]:

Got you. Cool. Well, listen, let me stop it there. Let me ask the three questions that we always end on. The first one is, when did you personally feel like you'd found true product market fit?

**Simon Eskildsen** [50:33]:

It was that day that Notion signed because Cursor was still not that big. They were an incredible customer, and we loved working with them. But when Notion signed, that's when I realized that this could be a really big business.

**Pablo Srugo** [50:46]:

Second question is, was there a point—I mean, things have gotten really fast, so I feel like the answer is no—but was there ever a point where you thought things just wouldn't actually work out?

**Simon Eskildsen** [50:53]:

About three months after we have gone to market, our biggest peer basically launched turbopuffer. That's when Justine and I had to look each other in the eye in January of 2024 and decide whether we were going to go up against this or if we're just going to go and do something else, right? We spent a lot of time debating that and ultimately decided that we're going to do our darn hardest here because we're excited and we have a great set of customers. But if we don't find it by the end of the year, we'll close up. We want to build a big database business. If we can't do that, then there's no point.

**Pablo Srugo** [51:29]:

By the way, how do you go up against that? Is that a product thing? Like your database is just better, or that's what you're trying to do? Just be better than them, be cheaper than them? Is there big ways to differentiate?

**Simon Eskildsen** [51:37]:

We thought that it was going to be incredibly difficult, right? When an engineer sees something, I think a good engineer will always assume that it's the best version of what it is. We knew what we were building, and we knew how to get to the best version of it, and we assumed that that's also what they had. But for some reason, customers kept coming inbound, right? Even though we were a small company. So like, why that happened? I don't know, right? Maybe it wasn't as suited for some of the workloads that they had optimized it for in R&D. Maybe they rushed it. I don't know, right? But I know that customers were testing us against it, and we were performing better, and that's why they chose us.

**Pablo Srugo** [52:13]:

And then last question, what would be like your number one piece of advice for other early-stage founders?

**Simon Eskildsen** [52:18]:

You are going to encounter a lot of very articulate advice from VCs and other advisors. But if you truly have the founder-like company fit or whatever, founder-market fit, you will really have to trust your intuition. That is a lesson that I've had to retell myself many, many times because it's very easy to be persuaded by someone who's very articulate and spend a lot of time in the market. You got to just lean into the things that make your business and what you think is right weird, right? And that's what makes your business unique. Otherwise, it becomes a cookie-cutter thing that comes out of Silicon Valley exactly as the VCs think that the playbook is. But every playbook that's worked has always been a little weird.

**Pablo Srugo** [52:59]:

Simon, thanks so much for jumping on the show, dude. It's been awesome.

**Simon Eskildsen** [53:02]:

Thank you, Pablo.

**Pablo Srugo** [53:03]:

Wow, what an episode. You're probably in awe. You're in absolute shock. You're like, "That helped me so much." So guess what? Now it's your turn to help someone else. Share the episode in the WhatsApp group you have with founders. Share it on that Slack channel. Send it to your founder friends and help them out. Trust me, they will love you for it.
