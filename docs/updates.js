// What's new and the update history, read from CHANGELOG.md so it is only ever written once.
// Every version shows in full: its summary line, then every point.
(() => {
  const SRC = 'https://raw.githubusercontent.com/kesavsabari/qh-rs/main/CHANGELOG.md';
  const boxes = document.querySelectorAll('[data-updates]');
  if (!boxes.length) return;

  const esc = s => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  const inline = s => esc(s)
    .replace(/\*\*(.+?)\*\*/g, '<b>$1</b>')
    .replace(/`([^`]+)`/g, '<code>$1</code>')
    .replace(/\[([^\]]+)\]\((https?:[^)\s]+)\)/g, '<a href="$2">$1</a>');

  // "## 0.1.6 — 2026-10-11", a summary line, then "- " points (continued on indented lines)
  function parse(md) {
    return md.split(/^## /m).slice(1).map(block => {
      const [head, ...rest] = block.split('\n');
      const [version, date = ''] = head.split(/\s+[—-]\s+/);
      const points = [], other = [];
      for (const line of rest) {
        if (line.startsWith('- ')) points.push(line.slice(2).trim());
        else if (/^\s+\S/.test(line) && points.length) points[points.length - 1] += ' ' + line.trim();
        else if (line.trim()) other.push(line.trim());
      }
      return { version: version.trim(), date: date.trim(), summary: other[0] || '', points };
    }).filter(v => /^\d/.test(v.version));  // skips "Unreleased"
  }

  const label = v => `<h3 class="upd-v"><span>${esc(v.version)}</span>${v.date ? `<time>${esc(v.date)}</time>` : ''}</h3>`;
  const full = v => `<article class="upd upd-full">${label(v)}${v.summary ? `<p class="upd-sum">${inline(v.summary)}</p>` : ''}<ul>${v.points.map(p => `<li>${inline(p)}</li>`).join('')}</ul></article>`;

  fetch(SRC, { cache: 'no-cache' }).then(r => r.ok ? r.text() : Promise.reject()).then(md => {
    const vs = parse(md);
    if (!vs.length) return;
    boxes.forEach(box => {
      box.innerHTML = box.dataset.updates === 'latest'
        ? full(vs[0])
        : vs.map(full).join('');  // every version in full, like the roadmap
    });
  }).catch(() => {});  // the link already in the box stays: the changelog on GitHub
})();
