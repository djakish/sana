# Training SID-1 to beat GPT-5 at search with 1k+ QPS RL

May 20, 2026•Max Rumpf (Co-founder of SID), Sam Dauncey (Researcher at SID)

Given sufficient search tools and time, humans can find almost anything. We search, read results, adapt, and search again until we find the information we seek.

We're Max and Sam, co-creators of [SID-1](https://sid.ai/research/sid-1-technical-report), an agentic search model that builds upon this idea. As a result of its training, SID-1 nearly doubles recall over classical retrieval pipelines and outperforms frontier LLMs at orders of magnitude lower latency and cost.

```
SID-1 performance

          model  recall                     time per question          cost per 1k questions
───────────────  ─────────────────────────  ─────────────────────────  ─────────────────────────

     SID-1 (4x)  │▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓ 0.84  │▓ 5.5s                    │▓ $1.40
                 │
 GPT-5.1 (high)  │░░░░░░░░░░░░░░░░ 0.78     │░░░░░░░░░░░░░░░░ 131s     │░░░░░░░░ $240
                 │
          SID-1  │▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓ 0.77     │▓ 5.5s                    │ $0.62
                 │
   Gemini 3 Pro  │░░░░░░░░░░░░░ 0.66        │░░░░░░░░░░░░░░░░░░░ 156s  │░░░░ $120
                 │
     Sonnet 4.5  │░░░░░░░░░░░░░ 0.64        │░░░░░ 35s                 │░░░░░░░░░░░░░░░░░░ $540
                 │
   Reranker @10  │░░░░░░░░░ 0.45            │ 0.78s                    │ $0.61
                 │
Vector only @10  │░░░░░░░░ 0.44             │ 0.15s                    │ $0.0098

source: sid.ai/research/sid-1
```

```
SID-1 performance
source: sid.ai/research/sid-1

recall          latency         cost

▓                     ░                 ░
▓ ░ ▓             ░   ░                 ░
▓ ░ ▓             ░   ░                 ░
▓ ░ ▓ ░ ░         ░   ░                 ░
▓ ░ ▓ ░ ░         ░   ░           ░     ░
▓ ░ ▓ ░ ░ ░ ░     ░   ░           ░     ░
▓ ░ ▓ ░ ░ ░ ░     ░   ░ ░         ░   ░ ░
▓ ░ ▓ ░ ░ ░ ░     ░   ░ ░         ░   ░ ░
▓ ░ ▓ ░ ░ ░ ░   ▓ ░ ▓ ░ ░       ▓ ░   ░ ░
─────────────   ─────────────   ─────────────
A B C D E F G   A B C D E F G   A B C D E F G

A: SID-1 (4x)
B: GPT-5.1 (high)
C: SID-1
D: Gemini 3 Pro
E: Sonnet 4.5
F: Reranker @10
G: Vector only @10
```

[SID](https://www.sid.ai/) is a research lab for search. We trained SID-1 using large-scale reinforcement learning (RL), and when training became bottlenecked on search latency, we migrated the search backend to turbopuffer. We wrote this post, on invitation from the turbopuffer team, to share how we train SID models using large-scale, synchronous RL rollouts at 1k+ searches per second over 10M+ document corpora across thousands of training steps.

## Iterative search > static retrieval

Unlike humans, static retrieval ("RAG") pipelines cannot search iteratively. They run a fixed sequence of steps and return the result, even when it's bad.

```
static retrieval pipeline

┌──────────┐  ┌─────────────┐  ┌─────────────┐  ┌──────────┐  ┌─────────┐
│ question ├─▶│ LLM rewrite ├─▶│ turbopuffer ├─▶│ reranker ├─▶│ results │
└──────────┘  └─────────────┘  └─────────────┘  └──────────┘  └─────────┘
                (optional)                       (optional)
```

```
static retrieval pipeline

 ┌─────────────┐
 │   question  │
 └──────┬──────┘
        ▼
 ┌─────────────┐
 │ LLM rewrite │
 │  (optional) │
 └──────┬──────┘
        ▼
 ┌─────────────┐
 │ turbopuffer │
 └──────┬──────┘
        ▼
 ┌─────────────┐
 │  reranker   │
 │ (optional)  │
 └──────┬──────┘
        ▼
 ┌─────────────┐
 │   results   │
 └─────────────┘
```

The conventional fixes are to add more retrieval steps (LLM query rewrites, hybrid search with rank fusion, reranking) or tweak the embedding model or chunking strategy, often at the cost of engineering time, complexity, and brittleness.

None of these fixes address the underlying problem: Every important decision is hard-coded once, at design time, and applied uniformly to all queries. No fixed set of choices is right for every question, which is why static pipelines often accumulate a long tail of failures.

**SID-1 treats search as an iterative process driven by an LLM**. It runs over multiple turns, calling tools to gather context until it has enough, then returns a ranked list of documents.

```
SID-1 retrieval pipeline

┌──────────┐    ┌─────────────┐           ┌─────────┐
│ question ├───▶│    SID-1    ├─ ranked ─▶│ results │
└──────────┘    └┬┬┬───────▲▲▲┘           └─────────┘
                 │││       │││
       tool calls│││   n   │││content +
   BM25, ANN, etc│││ turns │││metadata
                 │││       │││
                ┌▼▼▼───────┴┴┴┐
                │ turbopuffer │
                └─────────────┘
```

```
SID-1 retrieval pipeline

┌────────┐
│question│
└───┬────┘
┌───▼────┐          ┌──────┐
│ SID-1  ╞═══tools═▶│ tpuf │
│n-turns │◀═content═╡      │
└───┬────┘          └──────┘
  ranked
┌───▼────┐
│results │
└────────┘
```

This iterative process corrects the fundamental problem of static retrieval. Every design decision is now made by a model that adapts its approach to each query. Like a human, the model decides which tools to use, how to phrase queries, and when to stop searching. As a result, SID-1 outperforms classical embedding-reranking pipelines on recall.

This is not dissimilar to today's agentic search, where frontier LLMs progressively search and reason over new context. SID-1's training, however, makes it significantly more efficient than frontier LLMs at using search tools and reasoning across results, which is why SID-1 achieves higher recall than much slower and more expensive frontier models with the same expert prompting and harness.

This also makes SID-1 a strong subagent inside a frontier-model-led task. When a frontier model searches directly, every retrieved document and reasoning thought accumulates in its context. Incorrect documents pollute the context and unnecessarily waste tokens. SID-1 as a subagent gatekeeps context so only the best results reach the frontier model.

A frontier model with a 1M-token limit can see ~10k retrieved chunks (100 tokens) before saturating its context window. By calling SID-1 as a subagent in sequence or in parallel, the frontier model can feasibly reason over millions of documents.

```
┌─ agentic search ──────────────────────────────────────────────────────────┐
│                                                                           │
│  [user] ──▶ [frontier model] ──▶ [s][s][s] x ✓ x [s][s] ✓ x ──▶ [answer]  │
│                                            ↑   ↑          ↑               │
└────────────────────────────────────────────│───│──────────│───────────────┘
                                             context pollution

┌─ agentic search with SID-1 subagent ──────────────────────────────────────┐
│                                                                           │
│  [user] ──▶ [frontier model] ──▶ [SID-1] ✓ ✓ ✓ [SID-1] ✓ ✓ ───▶ [answer]  │
│                                  [s]           [s]                        │
│                                  [s]           [s]                        │
│                                  [s]            x                         │
│                                   ✓             x                         │
│                                   x             ✓                         │
│                                   ✓            [s]                        │
│                                  [s]            x                         │
│                                  [s]            ✓                         │
│                                   x                                       │
│                                   ✓                                       │
└───────────────────────────────────────────────────────────────────────────┘
```

```
agentic search
──────────────────────────────────────────────────
[user] ▶ [frontier] ▶ [s][s] x x [s] ✓ ▶ [answer]
                             ↑ ↑
                      context pollution

agentic search with SID-1 subagent
──────────────────────────────────────────────────
[user] ▶ [frontier] ▶ [SID] ✓ ✓ [SID] ✓ ▶ [answer]
                      [s]       [s]
                      [s]       [s]
                      [s]        x
                       ✓         x
                       x         ✓
                       ✓        [s]
                      [s]        x
                      [s]        ✓
                       x
                       ✓
```

## Training SID-1 with RL

We train SID-1 with a modified version of [GRPO](https://arxiv.org/abs/2402.03300), an RL algorithm first introduced by DeepSeek.

At each training step, we give the model 256 questions. Each question has a golden list of documents needed to answer it, drawn from a corpus that can range in scope from [5,000 manually curated abstracts](https://aclanthology.org/2020.emnlp-main.609/) to [the entire internet](https://blog.wilsonl.in/search-engine/).

The corpora span finance, science, legal, email, and general knowledge. Below is a sample question and correct documents from our training mix:

```
question

┃ What's the age gap between the TV producer who created the soap that premiered on the same
┃ night a major UK channel launched and the Prime Minister who represented the producer's
┃ hometown in Parliament?

correct documents
┌───────────────────────────────────────── Brookside ─────────────────────────────────────────┐
│ Brookside is a British soap opera set in Liverpool, England. The series began on the launch │░
│ night of Channel 4 on 2 November 1982, and ran for 21 years until 4 November 2003.          │░
│ Originally intended to be called "Meadowcroft", the series was produced by Mersey           │░
│ Television and it was conceived by Phil Redmond who also devised "Grange Hill" (1978–2008)  │░
│ and "Hollyoaks" (1995–present).                                                             │░
└─────────────────────────────────────────────────────────────────────────────────────────────┘░
 ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░

┌─────────────────────────────────────── Phil Redmond ────────────────────────────────────────┐
│ Philip Redmond CBE (born 10 June 1949) is an English television producer and screenwriter   │░
│ from Huyton, Lancashire.                                                                    │░
└─────────────────────────────────────────────────────────────────────────────────────────────┘░
 ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░

┌──────────────────────────── Huyton (UK Parliament constituency) ────────────────────────────┐
│ Huyton was a former constituency for the House of Commons. Created in 1950, it was centred  │░
│ on Huyton in Lancashire (later Merseyside), North West England, just beyond the borders of  │░
│ the city of Liverpool. The only MP was frontbench Labour politician, Harold Wilson who      │░
│ while representing the seat became Leader of the Labour Party in 1963 and Prime Minister    │░
│ from 1964-1970 and again from 1974-1976.                                                    │░
└─────────────────────────────────────────────────────────────────────────────────────────────┘░
 ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░

┌─────────────────────────────────────── Harold Wilson ───────────────────────────────────────┐
│ James Harold Wilson, Baron Wilson of Rievaulx, (11 March 1916 – 24 May 1995) was a British  │░
│ Labour Party politician who served as the Prime Minister of the United Kingdom from 1964 to │░
│ 1970 and 1974 to 1976.                                                                      │░
└─────────────────────────────────────────────────────────────────────────────────────────────┘░
 ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░
```

```
question

┃ What's the age gap between the TV producer
┃ who created the soap that premiered on
┃ the same night a major UK channel launched
┃ and the Prime Minister who represented
┃ the  producer's hometown in Parliament?

documents

┌────────── Brookside ──────────┐
│ British soap opera set in     │░
│ Liverpool. Premiered on the   │░
│ launch night of Channel 4 on  │░
│ 2 November 1982. Conceived by │░
│ Phil Redmond, who also made   │░
│ Grange Hill & Hollyoaks.      │░
└───────────────────────────────┘░
 ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░

┌──────── Phil Redmond ─────────┐
│ Philip Redmond CBE (b. 1949), │░
│ English TV producer & writer  │░
│ from Huyton, Lancashire.      │░
└───────────────────────────────┘░
 ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░

┌─────────── Huyton ────────────┐
│ Former UK Parliament          │░
│ constituency (created 1950),  │░
│ centred on Huyton, Lancashire │░
│ (later Merseyside), near      │░
│ Liverpool. Only MP was Harold │░
│ Wilson, who later became PM   │░
│ 1964-1970 & 1974-1976.        │░
└───────────────────────────────┘░
 ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░

┌──────── Harold Wilson ────────┐
│ James Harold Wilson, Baron    │░
│ Wilson of Rievaulx (11 March  │░
│ 1916 – 24 May 1995). British  │░
│ Labour politician; UK PM from │░
│ 1964-1970 & 1974-1976.        │░
└───────────────────────────────┘░
 ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░
```

The model gets 16 attempts per question, each containing multiple turns where the model can use the available search tools to fetch documents and reason over them.

If the model believes it has found everything or has exhausted its turns, it reports a ranked list of documents. Ranking allows SID-1 to seamlessly drop into existing search pipelines, mirroring `top_k` truncation in traditional retrieval.

We grade each attempt on whether it found the correct documents, ranked them correctly, and did so quickly.

```
G = 16 attempts per question q
╔════════════════════════════════════════════════════════════════╗
║  1. [q] ──▶ ━━━ [s][s] ━━━ [s][s][s] ━ ✓ ──▶ [reward=0.72]     ║═╗
║  2. [q] ──▶ ━ [s] ━━━━━ [s][s][s][s] ✓ ────▶ [reward=0.55]     ║ ║═╗
║  3. [q] ──▶ ━━━━━ [s][s] ━ [s] ━ [s][s] ✓ ─▶ [reward=0.83]     ║ ║ ║
║  4. [q] ──▶ ━━━ [s][s] ━━━━━ [s][s] ✓ ─────▶ [reward=0.61]     ║ ║ ║
║        ...                                                     ║ ║ ║
║ 15. [q] ──▶ ━ [s] ━━━ [s][s] ━ [s] ✓ ──────▶ [reward=0.48]     ║ ║ ║
║ 16. [q] ──▶ ━ [s] ━ [s][s] ━ [s] ✓ ────────▶ [reward=0.39]     ║ ║ ║
║                                                                ║ ║ ║
║  ━ think (length varies)   [s] search   ✓ report docs          ║ ║ ║
╚════════════════════════════════════════════════════════════════╝ ║ ║
  ╚════════════════════════════════════════════════════════════════╝ ║
    ╚════════════════════════════════════════════════════════════════╝
      ... B = 256 questions per batch
```

```
G = 16 attempts per question q

╔═════════════════════════════╗
║ 1 [q]━━[s][s]━[s][s][s]✓.72 ║═╗
║ 2 [q]━[s]━━[s][s][s][s]✓.55 ║ ║═╗
║ 3 [q]━[s][s]━[s]━[s][s]✓.83 ║ ║ ║
║ 4 [q]━━[s][s]━[s][s][s]✓.61 ║ ║ ║
║         ...                 ║ ║ ║
║15[q]━[s]━━[s][s]━[s]✓.48    ║ ║ ║
║16 [q]━[s]━[s][s]━[s]✓.39    ║ ║ ║
║                             ║ ║ ║
║ ━ think (length varies)     ║ ║ ║
║ [s] search                  ║ ║ ║
║ ✓ report docs               ║ ║ ║
╚═════════════════════════════╝ ║ ║
  ╚═════════════════════════════╝ ║
    ╚═════════════════════════════╝
     ... B = 256 questions per batch
```

At the end of each training step, GRPO compares all rewards across the 16 attempts and steers the model to behave more like the better-than-average attempts and less like the worse-than-average attempts.

### Tool selection

We do not tell SID-1 which search tools to use. It is free to choose whichever tool it finds most effective, building preferences through reinforcement.

On each turn, SID-1 chooses from a [suite of search tools](https://turbopuffer.com/docs/query) that turbopuffer exposes, including dense vector approximate nearest neighbor (ANN) search, BM25 full-text search, and metadata filtering.

As training progresses, SID-1 seems to prefer ANN over BM25. Across individual turns, we observe a similar but less strong trend; it slightly prefers ANN in the first turn, presumably to explore what the corpus contains. In later turns, it uses the information it has gathered so far to become more precise.

It also learns to natively use [hypothetical document embeddings (HyDE)](https://aclanthology.org/2023.acl-long.99/) later in the search process: rather than embedding the raw query, the model drafts a plausible answer document and embeds that as the search vector. This lands the search vector closer to real answer documents in embedding space, which can be highly effective when using ANN search.

Interestingly, SID-1 never abandons BM25 entirely, indicating that there are some tasks for which keyword search is uniquely suited. When performing keyword search, instead of guessing the perfect keyword string, the model may issue a mix of 3-4 overdetermined (narrow) and underdetermined (broad) searches at once. For example, it might perform a narrow search:

```
"TV producer created a soap that premiered on the same night a major UK channel launched"
```

...alongside broader queries like:

```
"UK TV channel launch date - BBC, ITV, Channel 4"
```

Learned tool preference opens an interesting avenue for studying search techniques more broadly: If RL makes a model prefer some tool, it is likely a better tool. As in other disciplines, like playing [Go](https://deepmind.google/research/alphago/), we expect future models to discover novel strategies that appear "alien" to experts, but outperform their designs.

### Speed and parallelism

It's important that the questions given to SID-1 during training aren't too hard or too easy. If the model never finds the right documents, all attempts get zero reward, and we can't tell which attempts were good. If all attempts score perfectly, we similarly don't know which behavior to reinforce.

As the model improves, therefore, the questions must get harder. Harder questions generally demand more searches; in later training stages, the model makes ~20 search tool invocations per attempt. Each turn demands 800-1400ms to think and generate tool calls. Assuming ~100ms search latency, a single search per turn equates to ~18-30s latency per attempt if all searches are performed sequentially. **This is far too slow to serve an end user when the model is deployed.**

Fortunately, because the model is rewarded on timeliness, parallelism emerges naturally during training: the model learns to issue 4-8 searches per turn rather than one. By searching in parallel, the model can see more documents in the same number of turns to more quickly arrive at an answer.

```
tool calls per turn increase over training steps

20 ┤                                                       ● tool calls
   │                                                     ●●  increase
   │                                                   ●●    (~5 → 20)
15 ┤                                               ●●●●
   │                                            ●●●
   │                                      ●●●●●●
10 ┤                            ●●●●●●●●●●
   │                    ●●●●●●●●
   │         ●●●●●●●●●●●
 5 ┤ ●●●●●●●                                                 turns stay
   │ ------------------------------------------------------- constant
   │                                                         (~3-4)
 0 ┴─┬──┬──┬──┬──┬──┬──┬──┬──┬──┬──┬──┬──┬──┬──┬──┬──┬──┬──┬─▶
                          training step

                ●●●●●●● tool calls    ------- turns
```

```
tool calls per turn over training

20 ┤                 ● tool calls
   │                ●  increase
15 ┤              ●●   (~5 → 20)
   │            ●●
   │          ●●
10 ┤       ●●●
   │   ●●●●
 5 ┤●●●                turns stay
   │------------------ constant
   │                   (~3-4)
 0 ┴─┬─┬─┬─┬─┬─┬─┬─┬─┬─▶
      training step

●●● tool calls
--- turns
```

This drops latency to ~5s on hard questions and ~1.5s on easy ones. The result is a model that is **~20x faster than frontier LLMs**, while also outperforming them on recall.

```
time spent per question, SID-1 vs frontier LLMs
─────────────────────────────────────────────────────────────────

  Gemini 3 Pro   │░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░ 156s
                 │
GPT-5.1 (high)   │░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░ 131s
                 │
    Sonnet 4.5   │░░░░░░░░░ 35s
                 │
         SID-1   │▓ 5.5s

source: sid.ai/research/sid-1
```

```
Time spent per question
SID-1 vs frontier LLMs

Gemini 3 Pro   │░░░░░░░░░░░░░░░░░░░ 156s
               │
GPT-5.1 (high) │░░░░░░░░░░░░░░░░ 131s
               │
Sonnet 4.5     │░░░░░ 35s
               │
SID-1          │▓ 5.5s

source:
sid.ai/research/sid-1
```

## Keeping GPUs hot

Speed and parallelism are great model features for the end-user, but they strain the search backend during training.

When training with synchronous RL, a single training step consists of generating all 256 questions over 16 attempts in parallel before updating the model weights. Given the model may make ~20 searches per attempt in later stages, we can have up to $256 \cdot 16 \cdot 20 \approx 81,920$ searches per step. We train for $\gg 1,000$ steps and average $> 100$ QPS across a step.

QPS is not monotonic across each training step, however. When training begins, all 4,096 attempts make their initial search requests within a very short window (~10s), leading to 1k+ QPS bursts.

```
search QPS during one training step

 QPS
  ▲
4k┤ ▓
  │ ▓
  │ ▓
  │ ▓ ▓
2k┤ ▓ ▓
  │ ▓ ▓ ▓
  │ ▓ ▓ ▓ ▓
  │ ▓ ▓ ▓ ▓ ▓ ▓ ▓ ▓
0 ┤ ▓ ▓ ▓ ▓ ▓ ▓ ▓ ▓ ▓ ▓ ▓ ▓ ▓ ▓ ▓ ▓ ▓ ▓ ▓ ▓ ...
  └─┬─────────────┬─────────┬───────┬─────────▶ time within step
    │             │         │       │
    fires first   turn 2    turn 3  turn 4
    tool calls
```

```
search QPS during step

QPS
  ▲
4k┤▓
  │▓
  │▓
  │▓
2k┤▓▓
  │▓▓▓
  │▓▓▓▓▓▓
  │▓▓▓▓▓▓▓▓▓▓▓▓
0 ┤▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓ ...
  └┬────────────────────┬─▶
   t=0      time    t=T_step
```

Some of the corpora have well over 10M records, and we intend to use even larger training environments in the future. **When you run thousands of synchronous queries over large search indexes, search latency quickly bottlenecks the time per step.**

A conventional search backend can only scale reads by replicating a large stateful index in memory across more nodes, each paid for continuously to absorb spikes that last only seconds per step. If the search database can only economically scale to 20 QPS, for example, processing 81,920 searches for each training step takes 4,096 seconds (~68 minutes). But the GPUs can run the model through all attempts about 10x faster than that (depending on hyperparameters and a few other things). Idling GPUs are an easy way to lose money.

## Scaling the search backend on turbopuffer

RL-for-search stresses a backend in ways that user-facing inference doesn't. The workload is bursty by construction, corpora change every time we improve the data pipeline, and the envelope keeps expanding to bigger corpora, more parallel tool calls, and more steps.

turbopuffer's architecture is well-adapted to absorb the shape and scale of our RL training traffic, allowing us to issue many more concurrent searches and make the most of our GPUs during training runs.

### Handling parallel, bursty reads

turbopuffer's query tier is a stateless layer on top of object storage, consisting of a caching hierarchy and compute. Decoupling compute from storage means read capacity isn't bottlenecked by a single machine per namespace.

```
╔═══turbopuffer region═════════════╗
                   ║      ┌─────────────────────────┐ ╠──┐
                   ║      │     ./tpuf indexer      │ ║░ │
                   ║      └─────────────────────────┘ ║░ │
                   ║      ┌─────────────────────────┐ ║░ │
                   ║      │     ./tpuf indexer      │ ║░ │
                   ║      └─────────────────────────┘ ║░ │   ╔═══Object Storage══════════════╗
                   ║                                  ║░ │   ║ ┏━━Indexing Queue━━━━━━━━━━━┓ ║░
                   ║      ┌─────────────────────────┐ ║░ │   ║ ┃■■■■■■■■■                  ┃ ║░
                   ║      │      ./tpuf query       │ ║░ │   ║ ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━┛ ║░
                   ║      │┌─Memory Cache──────────┐│ ║░ │   ║ ┏━/{org_id}/{namespace}━━━━━┓ ║░
                   ║      ││■■■■■■■■■■             ││ ║░ │   ║ ┃ ┏━/wal━━━━━━━━━━━━━━━━━━┓ ┃ ║░
                   ║   ┌─▶│└───────────────────────┘│ ║░ └──▶║ ┃ ┃■■■■■■■■■■■■■■■◈◈◈◈    ┃ ┃ ║░
                   ║   │  │┌─NVMe Cache────────────┐│ ║░     ║ ┃ ┗━━━━━━━━━━━━━━━━━━━━━━━┛ ┃ ║░
                   ║   │  ││■■■■■■■■■■■■■■■■■■■■■  ││ ║░ ┌──▶║ ┃ ┏━/index━━━━━━━━━━━━━━━━┓ ┃ ║░
                ┌──╩─┐ │  │└───────────────────────┘│ ║░ │   ║ ┃ ┃■■■■■■■■■■■■■■■        ┃ ┃ ║░
╔══════════╗    │    │ │  └─────────────────────────┘ ║░ │   ║ ┃ ┗━━━━━━━━━━━━━━━━━━━━━━━┛ ┃ ║░
║  Client  ║───▶│ LB │─┤  ┌─────────────────────────┐ ║░ │   ║ ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━┛ ║░
╚══════════╝░   │    │ │  │      ./tpuf query       │ ║░ │   ╚═══════════════════════════════╝░
 ░░░░░░░░░░░░   └──╦─┘ │  │┌─Memory Cache──────────┐│ ║░ │    ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░
                   ║   │  ││■■■■■■■■■■             ││ ╠──┘
                   ║   └─▶│└───────────────────────┘│ ║░
                   ║      │┌─NVMe Cache────────────┐│ ║░
                   ║      ││■■■■■■■■■■■■■■■■■■■■■  ││ ║░
                   ║      │└───────────────────────┘│ ║░
                   ║      └─────────────────────────┘ ║░
                   ╚══════════════════════════════════╝░
                    ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░
```

```
╔═══turbopuffer region═════════════╗
                   ║      ┌─────────────────────────┐ ╠──┐
                   ║      │     ./tpuf indexer      │ ║░ │
                   ║      └─────────────────────────┘ ║░ │
                   ║      ┌─────────────────────────┐ ║░ │
                   ║      │     ./tpuf indexer      │ ║░ │
                   ║      └─────────────────────────┘ ║░ │   ╔═══Object Storage══════════════╗
                   ║                                  ║░ │   ║ ┏━━Indexing Queue━━━━━━━━━━━┓ ║░
                   ║      ┌─────────────────────────┐ ║░ │   ║ ┃■■■■■■■■■                  ┃ ║░
                   ║      │      ./tpuf query       │ ║░ │   ║ ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━┛ ║░
                   ║      │┌─Memory Cache──────────┐│ ║░ │   ║ ┏━/{org_id}/{namespace}━━━━━┓ ║░
                   ║      ││■■■■■■■■■■             ││ ║░ │   ║ ┃ ┏━/wal━━━━━━━━━━━━━━━━━━┓ ┃ ║░
                   ║   ┌─▶│└───────────────────────┘│ ║░ └──▶║ ┃ ┃■■■■■■■■■■■■■■■◈◈◈◈    ┃ ┃ ║░
                   ║   │  │┌─NVMe Cache────────────┐│ ║░     ║ ┃ ┗━━━━━━━━━━━━━━━━━━━━━━━┛ ┃ ║░
                   ║   │  ││■■■■■■■■■■■■■■■■■■■■■  ││ ║░ ┌──▶║ ┃ ┏━/index━━━━━━━━━━━━━━━━┓ ┃ ║░
                ┌──╩─┐ │  │└───────────────────────┘│ ║░ │   ║ ┃ ┃■■■■■■■■■■■■■■■        ┃ ┃ ║░
╔══════════╗    │    │ │  └─────────────────────────┘ ║░ │   ║ ┃ ┗━━━━━━━━━━━━━━━━━━━━━━━┛ ┃ ║░
║  Client  ║───▶│ LB │─┤  ┌─────────────────────────┐ ║░ │   ║ ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━┛ ║░
╚══════════╝░   │    │ │  │      ./tpuf query       │ ║░ │   ╚═══════════════════════════════╝░
 ░░░░░░░░░░░░   └──╦─┘ │  │┌─Memory Cache──────────┐│ ║░ │    ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░
                   ║   │  ││■■■■■■■■■■             ││ ╠──┘
                   ║   └─▶│└───────────────────────┘│ ║░
                   ║      │┌─NVMe Cache────────────┐│ ║░
                   ║      ││■■■■■■■■■■■■■■■■■■■■■  ││ ║░
                   ║      │└───────────────────────┘│ ║░
                   ║      └─────────────────────────┘ ║░
                   ╚══════════════════════════════════╝░
                    ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░
```

Any query node with cache headroom can serve any query, and traffic for a hot namespace can spread across the shared pool within seconds rather than queueing on a single shard owner. turbopuffer further bin-packs namespaces onto shared compute nodes, so a namespace that spikes borrows capacity from whatever its neighbors are not using at that moment.

If absorbing a burst requires adding additional query nodes, the new nodes can hydrate cache from object storage on demand instead of having to fully copy a shard before they can serve traffic. This allows turbopuffer to be much more responsive to bursts compared to a compute-storage coupled architecture, where that same expansion involves shuffling large state between hosts – an operation that can take minutes to hours.

### Search tool diversity

RL pushes the model to explore creative search strategies that make the most of the toolbox it has. To provide the best search training, we need to use the most diverse set of tools possible. This becomes an engineering challenge: Building the search indexes for a large toolbox is a research project in and of itself.

turbopuffer builds all indexes under a [single query planner](https://turbopuffer.com/docs/query) that routes across them. We get performant BM25, dense vector, sparse vector, and regex search – combined with [native, index-aware filtering](https://turbopuffer.com/blog/native-filtering).

```
╔═ tpuf tool calls ════╗
                          ║  ┌───────────────┐   ║░
                         ┌╫─▶│      ANN      ├──┐║░
                         │║  ├───────────────┤  │║░
                         ├╫─▶│  ANN + filter ├──┤║░
                         │║  ├───────────────┤  │║░
┌──────────┐             ├╫─▶│      BM25     ├──┤║░               ┌──────────┐
│  SID-1   │             │║  ├───────────────┤  │║░               │  SID-1   │
│  turn n  │── fan out ──┼╫─▶│ BM25 + filter ├──┼╫── reasoning ──▶│ turn n+1 │
└──────────┘             │║  ├───────────────┤  │║░               └──────────┘
                         ├╫─▶│     regex     ├──┤║░
                         │║  ├───────────────┤  │║░
                         └╫─▶│     glob      ├──┘║░
                          ║  └───────────────┘   ║░
                          ╚══════════════════════╝░
                           ░░░░░░░░░░░░░░░░░░░░░░░░
```

```
┌──────────┐
      │  SID-1   │
      │  turn n  │
      └────┬─────┘
           │ fan out
           ▼
┌─tpuf tool calls──────┐
│ ANN                  │░
├──────────────────────┤░
│ ANN + filter         │░
├──────────────────────┤░
│ BM25                 │░
├──────────────────────┤░
│ BM25 + filter        │░
├──────────────────────┤░
│ regex                │░
├──────────────────────┤░
│ glob                 │░
└──────────┬───────────┘░
 ░░░░░░░░░░│░░░░░░░░░░░░░
           │ reasoning
           ▼
      ┌──────────┐
      │  SID-1   │
      │ turn n+1 │
      └──────────┘
```

### Scaling to extremely large namespaces

Scaling compute in the way SID-1 does makes many academic benchmarks too easy. This is not because search is solved, but because their "haystack" is too small: even the largest academic benchmarks only have <10M documents in their corpus. Scaling to larger corpora is thus mandatory for those who want to push the frontier of search. turbopuffer's public ceiling is [100 billion vectors in a single search index](https://turbopuffer.com/blog/ann-v3). This already gives us plenty of headroom, and we only expect this ceiling to rise.

Loading chart data...

### Scaling to zero when not training

Research workloads are inherently unpredictable. A training namespace can sit cold for weeks, and then we want it back online for an ablation. turbopuffer's object-storage-first storage engine only "puffs" actively queried namespaces into the cache, so inactive namespaces pay only the low cost of object storage. The economics work out to be roughly [100x cheaper than memory-resident vector databases for cold storage, and up to 20x cheaper for warm](https://turbopuffer.com/docs/tradeoffs). We can keep every retired training corpus online, so we can easily re-run ablations against the exact corpus that produced the last iteration. turbopuffer's [namespace pinning](https://turbopuffer.com/docs/pinning) further allows namespaces to be pinned to the cache throughout the training run. Data stays hot on reserved compute, so we get consistent performance with a much lower cost for our high query volumes during runs.

```
cost per GB per month, approximate

in-memory         | ████████████████████████████████████████████████ 100x (incumbents)
warm SSD cache    | ██████ ~6-20x
cold object store | █ 1x (baseline, turbopuffer at rest)
```

```
cost per GB per month

in-memory VDB (incumbents)
████████████████ 100x

warm SSD cache
██ ~6-20x

cold object store (turbopuffer)
█ 1x (baseline)
```

### Branching namespaces for corpora updates

Real-world corpora often change. This presents a challenge to RL-for-search practitioners: Corpus updates might remove a document that an existing training question relies on. The solution is to branch your corpus. turbopuffer natively supports [branching](https://turbopuffer.com/docs/branching), a constant-time operation that creates a new namespace sharing the parent's underlying storage. Subsequent upserts, patches, and deletes against the branch apply only to that branch, leaving the parent untouched, so we can evolve training corpora forward without invalidating the corpora older training questions were authored against.

## Putting it all together

Each of these qualities solves a specific failure mode we previously hit in our RL rollouts:

| RL-for-search need |  | turbopuffer |
| --- | --- | --- |
| Parallel, bursty reads | --> | Stateless readers, high QPS |
| Diverse toolbox | --> | ANN, BM25, regex, metadata filtering |
| Huge corpora | --> | 100B+ document namespaces |
| Scale down to zero | --> | Object-storage-native, pay-per-usage, pinning |
| Corpus updates | --> | Branching |

"Make the model better" and "make the backend carry the traffic" are two separate problems. In RL-for-search, the former depends upon the latter. turbopuffer gives us the latter without much engineering effort, so we can spend the majority of our time on the former. The output is a model that nearly doubles recall over traditional retrieval pipelines and beats much larger frontier LLMs at a fraction of their latency and cost.

We trained SID-1 using turbopuffer, and will continue to do so for future models. We recommend turbopuffer as a search backend for large-scale reinforcement learning: it enables >1k sustained QPS with capacity for bursts across 1B+ document namespaces, with cheap cold storage for inactive namespaces between training runs, and a diverse set of search tools for models to utilize. Our [technical report for SID-1](https://sid.ai/research/sid-1-technical-report) details our methodology and full benchmark. We are already training SID-2, which we expect will extend SID-1's speed and recall advantages even beyond the current generation of frontier LLMs. To try SID models in your retrieval pipeline, please join the [waitlist](https://tally.so/r/gDDVMd).
