// Replays the recorded call on the switchboard: lamps show who holds the one
// GPU trunk, cords patch between lines as ask / answer happen.
(() => {
  const svg = document.querySelector(".jacks");
  const lamps = n => svg.querySelector(`.line[data-line="${n}"] .lamp`);
  const trunk = svg.querySelector(".trunk-lamp");
  const holder = document.getElementById("holder");
  const ask = svg.querySelector(".cord-ask");
  const answer = svg.querySelector(".cord-answer");
  const log = [...document.querySelectorAll("#calllog li")];
  const names = { 1: "01 main", 2: "02 tests", 3: "03 docs", 4: "04 web" };

  const hold = n => {
    [1, 2, 3, 4].forEach(i => lamps(i).classList.toggle("on", i === n));
    trunk.classList.toggle("on", !!n);
    holder.textContent = n ? names[n] : "idle";
  };
  const lit = i => log.forEach((li, k) => li.classList.toggle("lit", k <= i));

  if (matchMedia("(prefers-reduced-motion: reduce)").matches) {
    ask.classList.add("spent"); answer.classList.add("up"); lit(log.length - 1); hold(1);
    return;
  }

  // [delay ms, action] — the order the recorded session ran in
  const script = [
    [0,    () => { ask.className.baseVal = "cord cord-ask"; answer.className.baseVal = "cord cord-answer"; lit(-1); hold(2); }],
    [1400, () => hold(3)],
    [1300, () => hold(4)],
    [1300, () => { hold(1); ask.classList.add("up"); lit(0); }],
    [1500, () => hold(2)],
    [1500, () => { ask.classList.replace("up", "spent"); answer.classList.add("up"); lit(1); }],
    [1500, () => { hold(1); lit(2); }],
    [2600, () => hold(0)],
    [2400, null],
  ];
  let i = 0, timer;
  const step = () => {
    const [, fn] = script[i];
    if (fn) fn();
    i = (i + 1) % script.length;
    timer = setTimeout(step, script[i][0] || 1200);
  };
  // only run while the board is on screen
  new IntersectionObserver(([e]) => {
    clearTimeout(timer);
    if (e.isIntersecting) step();
  }, { threshold: .25 }).observe(svg);
})();

// section lamps: only the section in view is lit
{
  const secs = document.querySelectorAll("main > section[id]");
  const io = new IntersectionObserver(es => es.forEach(e => e.target.classList.toggle("in-view", e.isIntersecting)),
                                      { rootMargin: "-45% 0px -45% 0px" });
  secs.forEach(s => io.observe(s));
}

document.querySelectorAll(".copy").forEach(btn => {
  const label = btn.querySelector("span");
  btn.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(btn.dataset.copy);
      btn.classList.add("done"); if (label) label.textContent = "Copied";
    } catch {
      if (label) label.textContent = "Press Ctrl+C";
      const code = btn.parentElement.querySelector("code");
      const r = document.createRange(); r.selectNodeContents(code);
      const s = getSelection(); s.removeAllRanges(); s.addRange(r);
    }
    setTimeout(() => { btn.classList.remove("done"); if (label) label.textContent = "Copy"; }, 1800);
  });
});



// bands: slide in when they reach the screen, count numbers up once
{
  document.documentElement.classList.add("js");
  const still = matchMedia("(prefers-reduced-motion: reduce)").matches;
  const count = el => {
    const from = +el.dataset.from, to = +el.dataset.count, t0 = performance.now(), dur = 1200;
    const step = t => { const k = Math.min(1, (t - t0) / dur); el.textContent = Math.round(from + (to - from) * (1 - Math.pow(1 - k, 3))); if (k < 1) requestAnimationFrame(step); };
    el.textContent = from; requestAnimationFrame(step);
  };
  const io = new IntersectionObserver(es => es.forEach(e => {
    if (!e.isIntersecting) return;
    e.target.classList.add("shown");
    if (!still) e.target.querySelectorAll("[data-count]").forEach(count);
    io.unobserve(e.target);
  }), { threshold: 0.35 });
  document.querySelectorAll(".band").forEach(b => io.observe(b));
}
