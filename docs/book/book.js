(function () {
  function selectAll(selector, root) {
    return Array.prototype.slice.call((root || document).querySelectorAll(selector));
  }

  function initLsmDemo(root) {
    var captions = [
      "A write enters the WAL first. The in-memory view can disappear; the WAL cannot.",
      "Indexing folds committed WAL batches into a new immutable L0 SST.",
      "Several L0 runs increase read work. Tiering merges runs into one higher-level run.",
      "Compaction keeps the newest record for each id and can discard tombstones once older runs are gone."
    ];
    var states = [
      [["wal: 7", "mem: a"], [], [], []],
      [[], ["L0: 7"], [], []],
      [[], ["L0: 7", "L0: 8", "L0: 9", "L0: 10"], [], []],
      [[], [], ["L1: 7-10"], ["tombstone dropped"]]
    ];
    var buttons = selectAll("[data-lsm-step]", root);
    var rows = selectAll("[data-lsm-row]", root);
    var caption = root.querySelector("[data-lsm-caption]");

    function render(step) {
      buttons.forEach(function (button) {
        button.setAttribute("aria-pressed", button.getAttribute("data-lsm-step") === String(step));
      });
      rows.forEach(function (row, rowIndex) {
        var cells = row.querySelector(".lsm-cells");
        cells.innerHTML = "";
        states[step][rowIndex].forEach(function (label) {
          var span = document.createElement("span");
          span.className = "cell";
          if (label.indexOf("wal") === 0 || label.indexOf("mem") === 0) span.className += " hot";
          if (label.indexOf("L1") === 0) span.className += " merge";
          if (label.indexOf("tombstone") === 0) span.className += " dead";
          span.textContent = label;
          cells.appendChild(span);
        });
      });
      caption.textContent = captions[step];
    }

    buttons.forEach(function (button) {
      button.addEventListener("click", function () {
        render(Number(button.getAttribute("data-lsm-step")));
      });
    });
    render(0);
  }

  function initSstDemo(root) {
    var input = root.querySelector("[data-sst-key]");
    var output = root.querySelector("[data-sst-output]");
    var blocks = selectAll("[data-sst-block]", root);
    var indexEntries = selectAll("[data-sst-index]", root);

    function render() {
      var key = Number(input.value);
      var blockIndex = key <= 19 ? 0 : key <= 39 ? 1 : key <= 59 ? 2 : 3;
      blocks.forEach(function (block) {
        block.classList.toggle("active", Number(block.getAttribute("data-sst-block")) === blockIndex);
      });
      indexEntries.forEach(function (entry) {
        entry.classList.toggle("active", Number(entry.getAttribute("data-sst-index")) === blockIndex);
      });
      output.textContent = "key " + key + " reads footer -> index entry " + blockIndex + " -> data block " + blockIndex;
    }

    input.addEventListener("input", render);
    render();
  }

  function initSimilarityDemo(root) {
    var canvas = root.querySelector("canvas");
    var angleInput = root.querySelector("[data-sim-angle]");
    var stat = root.querySelector("[data-sim-stat]");
    var ctx = canvas.getContext("2d");
    // Each item is a unit vector at a fixed angle (degrees). Items that mean
    // similar things sit at similar angles, so a small angle between two
    // vectors means "alike". This is the whole intuition behind embeddings.
    var items = [
      { label: "kitten", deg: 22, group: "animals" },
      { label: "cat", deg: 34, group: "animals" },
      { label: "puppy", deg: 58, group: "animals" },
      { label: "dog", deg: 70, group: "animals" },
      { label: "apple", deg: 134, group: "fruit" },
      { label: "banana", deg: 150, group: "fruit" },
      { label: "car", deg: 246, group: "vehicles" },
      { label: "truck", deg: 262, group: "vehicles" },
      { label: "bicycle", deg: 286, group: "vehicles" }
    ];
    var groupColor = { animals: "#8bd88b", fruit: "#f0c15a", vehicles: "#7eb7b1" };

    function rad(deg) {
      return (deg * Math.PI) / 180;
    }

    function resizeCanvas() {
      var rect = canvas.getBoundingClientRect();
      var ratio = window.devicePixelRatio || 1;
      canvas.width = Math.max(320, Math.floor(rect.width * ratio));
      canvas.height = Math.max(240, Math.floor(rect.height * ratio));
      ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
    }

    function render() {
      resizeCanvas();
      var w = canvas.clientWidth;
      var h = canvas.clientHeight;
      var cx = w / 2;
      var cy = h / 2;
      var r = Math.min(w, h) * 0.36;
      var queryDeg = Number(angleInput.value);
      var qa = rad(queryDeg);

      // Cosine similarity between two unit vectors is just cos(angle between).
      var ranked = items
        .map(function (it) {
          return { it: it, sim: Math.cos(rad(it.deg) - qa) };
        })
        .sort(function (a, b) {
          return b.sim - a.sim;
        });
      var topSet = {};
      ranked.slice(0, 3).forEach(function (x) {
        topSet[x.it.label] = true;
      });

      ctx.clearRect(0, 0, w, h);
      ctx.fillStyle = "#0a0a09";
      ctx.fillRect(0, 0, w, h);

      ctx.strokeStyle = "#2a2e33";
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.arc(cx, cy, r, 0, Math.PI * 2);
      ctx.stroke();

      // Query direction.
      var qx = cx + Math.cos(qa) * r;
      var qy = cy - Math.sin(qa) * r;
      ctx.strokeStyle = "#f3efe2";
      ctx.lineWidth = 2;
      ctx.beginPath();
      ctx.moveTo(cx, cy);
      ctx.lineTo(qx, qy);
      ctx.stroke();
      ctx.fillStyle = "#f3efe2";
      ctx.fillText("query", qx + 6, qy - 6);

      items.forEach(function (it) {
        var a = rad(it.deg);
        var px = cx + Math.cos(a) * r;
        var py = cy - Math.sin(a) * r;
        var hot = topSet[it.label];
        if (hot) {
          ctx.strokeStyle = "#3a4047";
          ctx.lineWidth = 1;
          ctx.beginPath();
          ctx.moveTo(cx, cy);
          ctx.lineTo(px, py);
          ctx.stroke();
        }
        ctx.fillStyle = hot ? groupColor[it.group] : "#494331";
        ctx.beginPath();
        ctx.arc(px, py, hot ? 6 : 4, 0, Math.PI * 2);
        ctx.fill();
        ctx.fillStyle = hot ? "#f3efe2" : "#777c7b";
        ctx.fillText(it.label, px + 8, py + 4);
      });

      var top = ranked.slice(0, 3).map(function (x) {
        return x.it.label + " " + x.sim.toFixed(2);
      });
      stat.textContent = "query angle=" + queryDeg + "deg  nearest: " + top.join("  ");
    }

    angleInput.addEventListener("input", render);
    window.addEventListener("resize", render);
    render();
  }

  function initWritePathDemo(root) {
    var captions = [
      "Validate and encode the batch in memory. Nothing durable exists yet. If the process dies now the write is simply lost — and that is correct, because the client was never told it succeeded.",
      "Stage the encoded bytes as an immutable wal_staging object. The bytes exist, but nothing points at them. A crash here leaves a harmless orphan, never a half-written record.",
      "Claim the next sequence number with a compare-and-set on wal_commit. Slot 7 is now reserved. Any other writer that sees this pending reservation finishes it before taking its own slot — so two writers can never grab the same number.",
      "Publish the canonical wal/0/7.wal object at its final name. The committed batch now exists, but the cursor has not advanced, so readers still ignore it.",
      "Compare-and-set the committed cursor forward to 7. Only now is the write durable and acknowledged to the client. The next query sees it through the overlay."
    ];
    var states = [
      ["", "7-…wal", "7-…wal", "7-…wal", "7-…wal"],
      ["", "", "pending 7", "pending 7", "committed 7"],
      ["", "", "", "7.wal", "7.wal"],
      ["no", "no", "reserved", "almost", "yes"]
    ];
    var buttons = selectAll("[data-write-step]", root);
    var rows = selectAll("[data-write-row]", root);
    var caption = root.querySelector("[data-write-caption]");

    function classFor(label) {
      if (label === "committed 7" || label === "yes") return "cell merge";
      if (label === "pending 7" || label === "reserved" || label === "almost") return "cell hot";
      if (label === "no") return "cell dead";
      return "cell";
    }

    function render(step) {
      buttons.forEach(function (button) {
        button.setAttribute("aria-pressed", button.getAttribute("data-write-step") === String(step));
      });
      rows.forEach(function (row, i) {
        var cells = row.querySelector(".lsm-cells");
        cells.innerHTML = "";
        var label = states[i][step];
        var span = document.createElement("span");
        span.className = label ? classFor(label) : "cell";
        span.textContent = label || "—";
        cells.appendChild(span);
      });
      caption.textContent = captions[step];
    }

    buttons.forEach(function (button) {
      button.addEventListener("click", function () {
        render(Number(button.getAttribute("data-write-step")));
      });
    });
    render(0);
  }

  function initLatencyDemo(root) {
    var track = root.querySelector("[data-lat-track]");
    var caption = root.querySelector("[data-latency-caption]");
    var buttons = selectAll("[data-latency-mode]", root);
    var modes = {
      hit: {
        total: 2.3,
        caption: "Cache hit: the manifest and the index blocks are already in this process. The object store is never touched. A couple of milliseconds, dominated by decoding and scoring.",
        segs: [
          ["network in", 0.3, "#7eb7b1"],
          ["cache read", 0.2, "#8bd88b"],
          ["decode + score", 1.5, "#c8b675"],
          ["network out", 0.3, "#7eb7b1"]
        ]
      },
      miss: {
        total: 30.1,
        caption: "Cache miss: Sana has to fetch objects from the store. On S3 each round trip is tens of milliseconds, and it dwarfs everything else. This is why Sana caches immutable objects, batches reads, and keeps a small WAL overlay instead of asking the store on every write.",
        segs: [
          ["network in", 0.3, "#7eb7b1"],
          ["object store", 28.0, "#c98888"],
          ["decode + score", 1.5, "#c8b675"],
          ["network out", 0.3, "#7eb7b1"]
        ]
      }
    };
    var scale = modes.miss.total;

    function render(mode) {
      var m = modes[mode];
      buttons.forEach(function (b) {
        b.setAttribute("aria-pressed", b.getAttribute("data-latency-mode") === mode);
      });
      track.innerHTML = "";
      m.segs.forEach(function (s) {
        var el = document.createElement("span");
        el.className = "lat-seg";
        el.style.width = (s[1] / scale) * 100 + "%";
        el.style.background = s[2];
        el.textContent = s[1] >= 2 ? s[0] + " " + s[1] + "ms" : "";
        el.title = s[0] + " " + s[1] + "ms";
        track.appendChild(el);
      });
      caption.textContent = m.caption + " (~" + m.total.toFixed(1) + " ms total)";
    }

    buttons.forEach(function (b) {
      b.addEventListener("click", function () {
        render(b.getAttribute("data-latency-mode"));
      });
    });
    render("miss");
  }

  function initVectorDemo(root) {
    var canvas = root.querySelector("canvas");
    var probeInput = root.querySelector("[data-probes]");
    var stat = root.querySelector("[data-vector-stat]");
    var ctx = canvas.getContext("2d");
    var centroids = [
      { x: 0.18, y: 0.22, color: "#8bd88b" },
      { x: 0.72, y: 0.26, color: "#70d6d0" },
      { x: 0.28, y: 0.73, color: "#f0c15a" },
      { x: 0.78, y: 0.72, color: "#e88989" }
    ];
    var points = [
      [0.13,0.18,0],[0.21,0.28,0],[0.26,0.18,0],[0.16,0.34,0],
      [0.63,0.19,1],[0.74,0.34,1],[0.83,0.22,1],[0.69,0.12,1],
      [0.19,0.66,2],[0.33,0.82,2],[0.38,0.64,2],[0.24,0.79,2],
      [0.69,0.66,3],[0.83,0.79,3],[0.76,0.58,3],[0.88,0.68,3]
    ];
    var query = { x: 0.58, y: 0.49 };

    function dist(a, b) {
      var dx = a.x - b.x;
      var dy = a.y - b.y;
      return Math.sqrt(dx * dx + dy * dy);
    }

    function resizeCanvas() {
      var rect = canvas.getBoundingClientRect();
      var ratio = window.devicePixelRatio || 1;
      canvas.width = Math.max(320, Math.floor(rect.width * ratio));
      canvas.height = Math.max(240, Math.floor(rect.height * ratio));
      ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
    }

    function xy(p) {
      return { x: p.x * canvas.clientWidth, y: p.y * canvas.clientHeight };
    }

    function render() {
      resizeCanvas();
      var w = canvas.clientWidth;
      var h = canvas.clientHeight;
      var probes = Number(probeInput.value);
      var ordered = centroids.map(function (c, i) {
        return { i: i, d: dist(query, c) };
      }).sort(function (a, b) {
        return a.d - b.d;
      });
      var active = ordered.slice(0, probes).map(function (x) { return x.i; });
      var activeMap = {};
      active.forEach(function (i) { activeMap[i] = true; });

      ctx.clearRect(0, 0, w, h);
      ctx.fillStyle = "#0a0a09";
      ctx.fillRect(0, 0, w, h);

      centroids.forEach(function (c, i) {
        var cxy = xy(c);
        ctx.strokeStyle = activeMap[i] ? c.color : "#363326";
        ctx.lineWidth = activeMap[i] ? 2 : 1;
        ctx.beginPath();
        ctx.arc(cxy.x, cxy.y, activeMap[i] ? 82 : 55, 0, Math.PI * 2);
        ctx.stroke();
      });

      points.forEach(function (p) {
        var c = centroids[p[2]];
        var px = p[0] * w;
        var py = p[1] * h;
        ctx.fillStyle = activeMap[p[2]] ? c.color : "#494331";
        ctx.beginPath();
        ctx.arc(px, py, activeMap[p[2]] ? 5 : 3, 0, Math.PI * 2);
        ctx.fill();
      });

      centroids.forEach(function (c, i) {
        var cxy = xy(c);
        ctx.fillStyle = c.color;
        ctx.strokeStyle = "#11110f";
        ctx.lineWidth = 2;
        ctx.beginPath();
        ctx.rect(cxy.x - 6, cxy.y - 6, 12, 12);
        ctx.fill();
        ctx.stroke();
        ctx.fillStyle = "#f3efe2";
        ctx.fillText("c" + i, cxy.x + 10, cxy.y - 8);
      });

      var qxy = xy(query);
      ctx.strokeStyle = "#f3efe2";
      ctx.lineWidth = 2;
      ctx.beginPath();
      ctx.moveTo(qxy.x - 8, qxy.y);
      ctx.lineTo(qxy.x + 8, qxy.y);
      ctx.moveTo(qxy.x, qxy.y - 8);
      ctx.lineTo(qxy.x, qxy.y + 8);
      ctx.stroke();
      ctx.fillStyle = "#f3efe2";
      ctx.fillText("q", qxy.x + 10, qxy.y + 4);

      var candidates = points.filter(function (p) { return activeMap[p[2]]; }).length;
      stat.textContent = "probes=" + probes + " clusters=" + active.join(",") + " candidates=" + candidates + " rerank=exact";
    }

    probeInput.addEventListener("input", render);
    window.addEventListener("resize", render);
    render();
  }

  document.addEventListener("DOMContentLoaded", function () {
    selectAll("[data-lsm-demo]").forEach(initLsmDemo);
    selectAll("[data-sst-demo]").forEach(initSstDemo);
    selectAll("[data-vector-demo]").forEach(initVectorDemo);
    selectAll("[data-similarity-demo]").forEach(initSimilarityDemo);
    selectAll("[data-writepath-demo]").forEach(initWritePathDemo);
    selectAll("[data-latency-demo]").forEach(initLatencyDemo);
  });
})();
