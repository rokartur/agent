(() => {
const slides = [...document.querySelectorAll(".slide")];
const tabs = [...document.querySelectorAll(".tabs a")];
const difference = document.getElementById("difference");
const pick = document.getElementById("pick");
const text = document.getElementById("text");
let current = Math.max(0, slides.findIndex((slide) => `#${slide.id}` === location.hash));
let sent = false;

function pickLabel() {
  return `Pick ${slides[current].dataset.mock}`;
}

// The mock is drawn at its design width and zoomed down to the window, never up.
function fitMock() {
  const view = slides[current].querySelector(".view");
  const mock = view.querySelector(".mock");
  mock.style.zoom = Math.min(1, view.clientWidth / parseFloat(mock.style.width));
}

function show(index) {
  const from = slides[current].querySelector(".view");
  const ratio = from.scrollTop / Math.max(1, from.scrollHeight - from.clientHeight);
  current = (index + slides.length) % slides.length;
  slides.forEach((slide, i) => { slide.hidden = i !== current; });
  tabs.forEach((tab, i) => tab.setAttribute("aria-current", String(i === current)));
  difference.textContent = slides[current].dataset.difference;
  if (!sent) pick.textContent = pickLabel();
  fitMock();
  const to = slides[current].querySelector(".view");
  to.scrollTop = ratio * (to.scrollHeight - to.clientHeight);
}

tabs.forEach((tab, i) => tab.addEventListener("click", (event) => { event.preventDefault(); show(i); }));
document.addEventListener("keydown", (event) => {
  // Keys inside a mock belong to the mock's own fields and buttons.
  if (event.target.closest(".mocks, input, textarea, [contenteditable]") || event.metaKey || event.ctrlKey || event.altKey) return;
  const index = Number(event.key) - 1;
  if (slides[index]) show(index);
  if (event.key === "ArrowLeft") show(current - 1);
  if (event.key === "ArrowRight") show(current + 1);
  // Enter on the focused Pick button already clicks it; on a focused tab it picks that tab's mock.
  if (event.key === "Enter" && event.target !== pick) {
    event.preventDefault();
    if (tabs.includes(event.target)) show(tabs.indexOf(event.target));
    pick.click();
  }
});
function send(answer, label) {
  sent = true;
  pick.disabled = text.disabled = true;
  pick.textContent = label;
  // agent.submit already told the user why the answer was not sent.
  agent.submit(answer).catch(() => {
    sent = false;
    pick.disabled = text.disabled = false;
    pick.textContent = pickLabel();
  });
}

pick.addEventListener("click", () => {
  const mock = slides[current].dataset.mock;
  const note = text.value.trim();
  send(note ? { mock, text: note } : { mock }, `Picked ${mock}`);
});
text.addEventListener("keydown", (event) => {
  if (event.key !== "Enter" || !text.value.trim()) return;
  event.preventDefault();
  send({ text: text.value.trim() }, "Sent");
});
new ResizeObserver(fitMock).observe(document.querySelector(".mocks"));
show(current);
})();
