// The shapes follow the Arc chart components (uiarc.dev): the same type names and data fields.
(() => {
const CHART_TYPES = {
  bar: barChart, line: lineChart, sparkline, donut: donutChart, gauge, radar: radarChart, waffle: waffleChart,
  slope: slopeChart, "split-bar": splitBar, streamgraph, treemap, ridgeline, heatmap: activityHeatmap,
  funnel: funnelChart, sunburst, race: raceBarChart, brush: brushChart, stream: realtimeStream, sankey: sankeyFlow,
};
const GUTTER = 52;
const AXIS = 26;
const SHADES = [80, 56, 42, 32, 24, 18];
const TAU = Math.PI * 2;
const TONES = { accent: "var(--accent)", success: "var(--success)", warning: "var(--warning)", danger: "var(--danger)" };
const compact = new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 1 });

agent.chart = function chart(figure, spec) {
  figure.stopChart?.();
  figure.classList.add("chart");
  let width = 0;
  let stop;
  const draw = () => {
    if (figure.clientWidth === width) return;
    // Only the first draw animates; a resize redraws in place.
    if (width) figure.classList.add("again");
    width = figure.clientWidth;
    stop?.();
    figure.textContent = "";
    try {
      const render = CHART_TYPES[spec.type];
      if (!render) throw new Error(`unknown type, use one of ${Object.keys(CHART_TYPES).join(", ")}`);
      stop = render(figure, spec, width);
    } catch (error) {
      figure.innerHTML = `<p class="chart-error">chart ${esc(spec.type)}: ${esc(error.message)}</p>`;
    }
  };
  const observer = new ResizeObserver(draw);
  observer.observe(figure);
  const hover = new AbortController();
  draw();
  wireHover(figure, hover.signal);
  figure.stopChart = () => {
    observer.disconnect();
    hover.abort();
    stop?.();
  };
};

addEventListener("DOMContentLoaded", () => {
  for (const figure of document.querySelectorAll("figure[data-chart]")) {
    let spec;
    try {
      spec = JSON.parse(figure.dataset.chart);
    } catch (error) {
      figure.innerHTML = `<p class="chart-error">data-chart is not JSON: ${esc(error.message)}</p>`;
      continue;
    }
    agent.chart(figure, spec);
  }
});

function esc(text) {
  return String(text ?? "").replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}

function shade(index) {
  return `color-mix(in oklab, var(--foreground) ${SHADES[index % SHADES.length]}%, var(--surface))`;
}

function colorOf(item, index) {
  return item.color === undefined ? shade(index) : esc(item.color);
}

function colored(items) {
  return items.map((item, i) => ({ ...item, color: colorOf(item, i) }));
}

function percent(part, whole, decimals = 1) {
  return `${((part / whole) * 100).toFixed(decimals)}%`;
}

function sum(values) {
  let total = 0;
  for (const value of values) total += Number(value ?? 0);
  return total;
}

function formatter(spec) {
  const number = new Intl.NumberFormat("en-US", spec.format ?? { maximumFractionDigits: 1 });
  return number.format;
}

function withUnit(spec, text) {
  return spec.unit ? `${text} ${spec.unit}` : text;
}

// The smallest step of 1, 2, 2.5 or 5 that covers the data in four rows or fewer, from Arc's bar chart.
function niceScale(low, high) {
  low = Math.min(0, low);
  high = Math.max(high, low + 1);
  const magnitude = 10 ** Math.floor(Math.log10((high - low) / 4));
  for (const factor of [1, 2, 2.5, 5, 10, 20]) {
    const step = factor * magnitude;
    const from = Math.floor(low / step) * step;
    const to = Math.ceil(high / step) * step;
    if ((to - from) / step <= 4) {
      const ticks = [];
      for (let tick = from; tick <= to + step / 2; tick += step) ticks.push(tick);
      return { from, to, ticks };
    }
  }
  throw new Error("the data has no finite values");
}

function svgOpen(width, height, label) {
  return `<svg width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" role="img" aria-label="${esc(label)}">`;
}

function head(figure, spec, value, detail) {
  figure.insertAdjacentHTML(
    "beforeend",
    `<div class="chart-head"><span class="chart-label">${esc(spec.label)}</span><span class="chart-value"><span>${esc(value)}</span>${spec.unit ? `<small>${esc(spec.unit)}</small>` : ""}</span><span class="chart-detail">${esc(detail)}</span></div>`,
  );
}

function legend(items, extraClass = "") {
  let html = `<div class="chart-legend ${extraClass}">`;
  for (const item of items) {
    html += `<span class="mark" data-k="${esc(item.key)}" style="--c: ${item.color}"><i></i>${esc(item.label)}${item.value === undefined ? "" : ` <b>${esc(item.value)}</b>`}</span>`;
  }
  return `${html}</div>`;
}

// Hovering anything with data-k lights its marks and dims the rest; data-v and data-d replace the headline, data-tip floats.
function wireHover(figure, signal) {
  let tip = document.querySelector(".chart-tip");
  if (!tip) {
    tip = document.createElement("div");
    tip.className = "chart-tip";
    document.body.append(tip);
  }
  let lastKey;
  const clear = () => {
    figure.classList.remove("scrub");
    for (const mark of figure.querySelectorAll(".mark.on")) mark.classList.remove("on");
    showHeadline(figure);
    tip.classList.remove("on");
    lastKey = undefined;
  };
  figure.addEventListener("pointerleave", clear, { signal });
  figure.addEventListener("pointermove", (event) => {
    const hit = event.target.closest("[data-k], [data-tip], [data-v]");
    if (!hit || !figure.contains(hit)) return clear();
    const key = hit.dataset.k;
    if (key !== lastKey) {
      lastKey = key;
      figure.classList.toggle("scrub", key !== undefined);
      for (const mark of figure.querySelectorAll(".mark")) mark.classList.toggle("on", mark.dataset.k === key);
    }
    showHeadline(figure, hit.dataset.v, hit.dataset.d);
    if (!hit.dataset.tip) return tip.classList.remove("on");
    tip.textContent = hit.dataset.tip;
    const x = Math.min(event.clientX + 14, innerWidth - tip.offsetWidth - 8);
    const y = event.clientY + 14 + tip.offsetHeight > innerHeight ? event.clientY - tip.offsetHeight - 10 : event.clientY + 14;
    tip.style.left = `${x}px`;
    tip.style.top = `${y}px`;
    tip.classList.add("on");
  }, { signal });
}

function showHeadline(figure, value, detail) {
  const valueText = figure.querySelector(".chart-value > span");
  const detailText = figure.querySelector(".chart-detail");
  if (!valueText) return;
  valueText.dataset.rest ??= valueText.textContent;
  detailText.dataset.rest ??= detailText.textContent;
  valueText.textContent = value ?? valueText.dataset.rest;
  detailText.textContent = detail ?? detailText.dataset.rest;
}

function hoverAttributes({ key, value, detail, tip }) {
  let attributes = "";
  if (key !== undefined) attributes += ` data-k="${esc(key)}"`;
  if (value !== undefined) attributes += ` data-v="${esc(value)}"`;
  if (detail !== undefined) attributes += ` data-d="${esc(detail)}"`;
  if (tip !== undefined) attributes += ` data-tip="${esc(tip)}"`;
  return attributes;
}

// Monotone cubic through the points (Fritsch-Carlson), so a smooth line never overshoots a value.
function curvePath(points, smooth = true) {
  if (points.length === 0) return "";
  let d = `M${points[0][0].toFixed(2)} ${points[0][1].toFixed(2)}`;
  if (!smooth || points.length < 3) {
    for (const [x, y] of points.slice(1)) d += `L${x.toFixed(2)} ${y.toFixed(2)}`;
    return d;
  }
  const slopes = [];
  for (let i = 0; i < points.length - 1; i++) {
    slopes.push((points[i + 1][1] - points[i][1]) / (points[i + 1][0] - points[i][0] || 1));
  }
  const tangents = [slopes[0]];
  for (let i = 1; i < points.length - 1; i++) {
    tangents.push(slopes[i - 1] * slopes[i] <= 0 ? 0 : (slopes[i - 1] + slopes[i]) / 2);
  }
  tangents.push(slopes[slopes.length - 1]);
  for (let i = 0; i < slopes.length; i++) {
    if (slopes[i] === 0) {
      tangents[i] = 0;
      tangents[i + 1] = 0;
      continue;
    }
    const a = tangents[i] / slopes[i];
    const b = tangents[i + 1] / slopes[i];
    const length = Math.hypot(a, b);
    if (length > 3) {
      tangents[i] = (3 * a * slopes[i]) / length;
      tangents[i + 1] = (3 * b * slopes[i]) / length;
    }
  }
  for (let i = 0; i < points.length - 1; i++) {
    const [x0, y0] = points[i];
    const [x1, y1] = points[i + 1];
    const dx = (x1 - x0) / 3;
    d += `C${(x0 + dx).toFixed(2)} ${(y0 + dx * tangents[i]).toFixed(2)} ${(x1 - dx).toFixed(2)} ${(y1 - dx * tangents[i + 1]).toFixed(2)} ${x1.toFixed(2)} ${y1.toFixed(2)}`;
  }
  return d;
}

// A transparent full-height column that shows `hint` (a guide line, dots) while hovered.
function hoverColumn(x, slot, height, hover, hint) {
  return `<g class="x-col"${hoverAttributes(hover)}><rect x="${x - slot / 2}" y="0" width="${slot}" height="${height}" fill="transparent"/><g class="x-hint">${hint}</g></g>`;
}

function guide(x, top, bottom) {
  return `<line x1="${x}" x2="${x}" y1="${top}" y2="${bottom}" stroke="var(--border-strong)"/>`;
}

function dot(x, y, color, radius = 4) {
  return `<circle cx="${x}" cy="${y}" r="${radius}" fill="${color}" stroke="var(--background)" stroke-width="2"/>`;
}

function areaPath(path, left, right, floor) {
  return `${path}L${right} ${floor}L${left} ${floor}Z`;
}

function yAxis(scale, plotWidth, top, height) {
  const y = (value) => top + height - ((value - scale.from) / (scale.to - scale.from)) * height;
  let html = "";
  for (const tick of scale.ticks) {
    const line = Math.round(y(tick)) + 0.5;
    html += `<line class="${tick === 0 ? "base-line" : "grid-line"}" x1="0" x2="${plotWidth}" y1="${line}" y2="${line}"/>`;
    html += `<text x="${plotWidth + 10}" y="${line + 4}">${compact.format(tick)}</text>`;
  }
  return { html, y };
}

function xAxis(labels, xOf, y) {
  const LABEL_WIDTH = 64;
  const every = Math.max(1, Math.ceil((labels.length * LABEL_WIDTH) / Math.max(1, xOf(labels.length - 1) - xOf(0) + LABEL_WIDTH)));
  let html = "";
  for (let i = 0; i < labels.length; i += every) {
    const anchor = i === 0 && labels.length > 1 ? "start" : i === labels.length - 1 ? "end" : "middle";
    html += `<text x="${xOf(i)}" y="${y}" text-anchor="${labels.length === 1 ? "middle" : anchor}">${esc(labels[i])}</text>`;
  }
  return html;
}

function arcPoint(cx, cy, radius, angle) {
  return [cx + radius * Math.cos(angle), cy + radius * Math.sin(angle)];
}

function ringPath(cx, cy, inner, outer, start, end) {
  const large = end - start > Math.PI ? 1 : 0;
  const [ax, ay] = arcPoint(cx, cy, outer, start);
  const [bx, by] = arcPoint(cx, cy, outer, end);
  const [ix, iy] = arcPoint(cx, cy, inner, end);
  const [jx, jy] = arcPoint(cx, cy, inner, start);
  if (inner <= 0) return `M${cx} ${cy}L${ax} ${ay}A${outer} ${outer} 0 ${large} 1 ${bx} ${by}Z`;
  return `M${ax} ${ay}A${outer} ${outer} 0 ${large} 1 ${bx} ${by}L${ix} ${iy}A${inner} ${inner} 0 ${large} 0 ${jx} ${jy}Z`;
}

function barPath(x, width, height, base) {
  if (height <= 0.01) return `M${x} ${base}H${x + width}Z`;
  const r = Math.min(4, width / 2, height);
  const top = base - height;
  return `M${x} ${base}V${top + r}A${r} ${r} 0 0 1 ${x + r} ${top}H${x + width - r}A${r} ${r} 0 0 1 ${x + width} ${top + r}V${base}Z`;
}

// Pushes sorted label positions apart by at least `gap`, then back inside the bounds.
function spread(positions, gap, low, high) {
  const order = positions.map((position, index) => ({ position, index })).sort((a, b) => a.position - b.position);
  for (let i = 1; i < order.length; i++) order[i].position = Math.max(order[i].position, order[i - 1].position + gap);
  const overflow = order.length ? order[order.length - 1].position - high : 0;
  if (overflow > 0) for (const item of order) item.position -= overflow;
  for (let i = 0; i < order.length; i++) order[i].position = Math.max(order[i].position, low + i * gap);
  const result = [];
  for (const item of order) result[item.index] = item.position;
  return result;
}

function barChart(figure, spec, width) {
  const data = spec.data;
  const format = formatter(spec);
  const values = data.map((datum) => datum.value);
  head(figure, spec, spec.value ?? format(sum(values)), spec.period ?? "");
  const height = spec.height ?? 220;
  const plotWidth = width - GUTTER;
  const scale = niceScale(Math.min(...values), Math.max(...values));
  const axis = yAxis(scale, plotWidth, 12, height - 12);
  const slot = plotWidth / data.length;
  const barWidth = Math.min(slot * 0.58, 28);
  let marks = "";
  data.forEach((datum, i) => {
    const x = i * slot + (slot - barWidth) / 2;
    const top = axis.y(Math.max(datum.value, 0));
    const base = axis.y(Math.min(datum.value, 0));
    marks += `<path class="mark grow" style="animation-delay: ${(i / data.length) * 0.25}s; fill: color-mix(in oklab, var(--accent) 76%, var(--surface))" data-k="${i}" d="${barPath(x, barWidth, base - top, base)}"/>`;
    marks += `<rect x="${i * slot}" y="0" width="${slot}" height="${height}" fill="transparent"${hoverAttributes({ key: i, value: format(datum.value), detail: datum.label })}/>`;
  });
  if (spec.showAverage) {
    const average = axis.y(sum(values) / values.length);
    marks += `<line x1="0" x2="${plotWidth}" y1="${average}" y2="${average}" stroke="var(--secondary)" stroke-dasharray="3 3"/>`;
    marks += `<text x="${plotWidth + 10}" y="${average + 4}" style="fill: var(--foreground)">${esc(spec.averageLabel ?? "avg")}</text>`;
  }
  const labels = xAxis(data.map((datum) => datum.axisLabel ?? datum.label), (i) => i * slot + slot / 2, height + 18);
  figure.insertAdjacentHTML("beforeend", `${svgOpen(width, height + AXIS, spec.label)}${axis.html}${marks}${labels}</svg>`);
}

function lineChart(figure, spec, width) {
  const data = spec.data;
  const series = colored(spec.series);
  const format = formatter(spec);
  const first = series[0];
  const last = data[data.length - 1];
  head(figure, spec, spec.value ?? format(last.values[first.key] ?? 0), spec.period ?? last.label);
  const height = spec.height ?? 240;
  const plotWidth = width - GUTTER;
  const all = data.flatMap((datum) => series.map((line) => datum.values[line.key]).filter((value) => value !== undefined));
  const scale = niceScale(Math.min(...all), Math.max(...all));
  const axis = yAxis(scale, plotWidth, 12, height - 12);
  const xOf = (i) => (data.length === 1 ? plotWidth / 2 : (i / (data.length - 1)) * plotWidth);
  let marks = "";
  for (const line of series) {
    const points = [];
    data.forEach((datum, i) => {
      if (datum.values[line.key] !== undefined) points.push([xOf(i), axis.y(datum.values[line.key])]);
    });
    const path = curvePath(points, spec.curve !== "linear");
    if (line.area && points.length) {
      const floor = axis.y(Math.max(scale.from, 0));
      marks += `<path class="mark fade" data-k="${esc(line.key)}" fill="color-mix(in oklab, ${line.color} 16%, transparent)" d="${areaPath(path, points[0][0], points[points.length - 1][0], floor)}"/>`;
    }
    marks += `<path class="mark draw" pathLength="1" data-k="${esc(line.key)}" fill="none" stroke="${line.color}" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"${line.dashed ? ' style="stroke-dasharray: 4 4; stroke-dashoffset: 0; animation: none"' : ""} d="${path}"/>`;
  }
  const slot = plotWidth / Math.max(1, data.length - 1);
  data.forEach((datum, i) => {
    let dots = "";
    const tip = [];
    for (const line of series) {
      const value = datum.values[line.key];
      if (value === undefined) continue;
      dots += dot(xOf(i), axis.y(value), line.color);
      tip.push(`${line.label}  ${withUnit(spec, format(value))}`);
    }
    const value = datum.values[first.key];
    const hover = { value: value === undefined ? undefined : format(value), detail: datum.label, tip: series.length > 1 ? tip.join("\n") : undefined };
    marks += hoverColumn(xOf(i), slot, height, hover, `${guide(xOf(i), 12, height)}${dots}`);
  });
  const labels = xAxis(data.map((datum) => datum.axisLabel ?? datum.label), xOf, height + 18);
  figure.insertAdjacentHTML("beforeend", `${svgOpen(width, height + AXIS, spec.label)}${axis.html}${marks}${labels}</svg>`);
  if (series.length > 1 && spec.legend !== false) figure.insertAdjacentHTML("beforeend", legend(series));
}

function sparkline(figure, spec, width) {
  const data = spec.data;
  const format = formatter(spec);
  const tone = TONES[spec.tone ?? "accent"];
  const changeTone = spec.tone && spec.tone !== "accent" ? tone : "var(--secondary)";
  figure.insertAdjacentHTML(
    "beforeend",
    `<div class="chart-head" style="grid-template-columns: 1fr auto; align-items: baseline"><span class="chart-label">${esc(spec.label)}</span><span class="chart-detail" style="color: ${changeTone}; text-align: right">${esc(spec.change ?? "")}</span><span class="chart-value" style="grid-column: 1 / -1"><span>${esc(spec.value ?? format(data[data.length - 1]))}</span>${spec.unit ? `<small>${esc(spec.unit)}</small>` : ""}</span></div>`,
  );
  const height = spec.height ?? 48;
  const inset = 5;
  const low = Math.min(...data);
  const high = Math.max(...data);
  const xOf = (i) => inset + (i / Math.max(1, data.length - 1)) * (width - inset * 2);
  const yOf = (value) => inset + (1 - (value - low) / (high - low || 1)) * (height - inset * 2);
  const points = data.map((value, i) => [xOf(i), yOf(value)]);
  const path = curvePath(points);
  let marks = "";
  if (spec.area !== false) {
    marks += `<path class="fade" fill="color-mix(in oklab, ${tone} 14%, transparent)" d="${areaPath(path, xOf(0), xOf(data.length - 1), height)}"/>`;
  }
  marks += `<path class="draw" pathLength="1" fill="none" stroke="${tone}" stroke-width="1.75" stroke-linecap="round" d="${path}"/>`;
  const slot = width / Math.max(1, data.length - 1);
  data.forEach((value, i) => {
    marks += hoverColumn(xOf(i), slot, height, { value: format(value), detail: spec.labels?.[i] ?? "" }, dot(xOf(i), yOf(value), tone, 3.5));
  });
  figure.insertAdjacentHTML("beforeend", `${svgOpen(width, height, spec.label)}${marks}</svg>`);
}

function donutChart(figure, spec, width) {
  const format = formatter(spec);
  let data = spec.data.filter((datum) => datum.value > 0).sort((a, b) => b.value - a.value);
  const limit = spec.maxSegments ?? 6;
  if (data.length > limit) {
    const rest = data.slice(limit - 1);
    data = [...data.slice(0, limit - 1), { key: "__other", label: spec.otherLabel ?? "Other", value: sum(rest.map((datum) => datum.value)), color: shade(4) }];
  }
  const total = sum(data.map((datum) => datum.value));
  head(figure, spec, format(total), spec.totalLabel ?? "Total");
  const size = Math.min(spec.size ?? 200, width);
  const outer = size / 2 - 7;
  const inner = outer - (spec.thickness ?? 26);
  const gap = data.length > 1 ? 3 / outer : 0;
  let angle = -Math.PI / 2;
  let marks = "";
  data.forEach((datum, i) => {
    const sweep = (datum.value / total) * TAU;
    const end = angle + sweep - gap;
    const share = percent(datum.value, total);
    const path = sweep >= TAU - 0.001 ? `${ringPath(size / 2, size / 2, inner, outer, 0, Math.PI)}${ringPath(size / 2, size / 2, inner, outer, Math.PI, TAU)}` : ringPath(size / 2, size / 2, inner, outer, angle, Math.max(angle + 0.001, end));
    marks += `<path class="mark fade" style="animation-delay: ${i * 0.05}s" fill="${colorOf(datum, i)}" d="${path}"${hoverAttributes({ key: datum.key, value: format(datum.value), detail: `${datum.label}, ${share}` })}/>`;
    angle += sweep;
  });
  const items = data.map((datum, i) => ({ key: datum.key, label: datum.label, color: colorOf(datum, i), value: percent(datum.value, total, 0) }));
  figure.insertAdjacentHTML(
    "beforeend",
    `<div class="chart-row">${svgOpen(size, size, spec.label)}${marks}</svg>${spec.legend === false ? "" : legend(items, "chart-legend-column")}</div>`,
  );
}

function gauge(figure, spec, width) {
  const min = spec.min ?? 0;
  const max = spec.max ?? 100;
  const format = formatter(spec);
  const share = Math.min(1, Math.max(0, (spec.value - min) / (max - min || 1)));
  let band;
  const thresholds = [...(spec.thresholds ?? [])].sort((a, b) => a.from - b.from);
  for (const threshold of thresholds) if (spec.value >= threshold.from) band = threshold;
  const color = TONES[band?.tone ?? spec.tone ?? "accent"];
  const size = Math.min(220, width);
  const radius = size / 2 - 12;
  const start = (-225 * Math.PI) / 180;
  const arc = (to) => {
    const [ax, ay] = arcPoint(size / 2, size / 2, radius, start);
    const [bx, by] = arcPoint(size / 2, size / 2, radius, start + to * (Math.PI * 1.5));
    return `M${ax} ${ay}A${radius} ${radius} 0 ${to * 270 > 180 ? 1 : 0} 1 ${bx} ${by}`;
  };
  let marks = `<path d="${arc(1)}" fill="none" stroke="var(--surface-muted)" stroke-width="12" stroke-linecap="round"/>`;
  if (share > 0) marks += `<path class="draw" pathLength="1" d="${arc(share)}" fill="none" stroke="${color}" stroke-width="12" stroke-linecap="round"/>`;
  marks += `<text x="${size / 2}" y="${size / 2 + 8}" text-anchor="middle" style="font: 500 34px var(--display); fill: var(--foreground)">${esc(format(spec.value))}${spec.unit ? `<tspan style="font: 16px var(--sans); fill: var(--secondary)"> ${esc(spec.unit)}</tspan>` : ""}</text>`;
  marks += `<text x="${size / 2}" y="${size / 2 + 32}" text-anchor="middle" style="fill: ${band ? color : "var(--muted)"}">${esc(band?.label ?? spec.detail ?? "")}</text>`;
  marks += `<text x="${arcPoint(size / 2, size / 2, radius, start)[0]}" y="${size - 4}" text-anchor="middle">${esc(compact.format(min))}</text>`;
  marks += `<text x="${arcPoint(size / 2, size / 2, radius, start + Math.PI * 1.5)[0]}" y="${size - 4}" text-anchor="middle">${esc(compact.format(max))}</text>`;
  figure.insertAdjacentHTML("beforeend", `<span class="chart-label">${esc(spec.label)}</span>${svgOpen(size, size, `${spec.label}: ${format(spec.value)}`)}${marks}</svg>`);
}

function radarChart(figure, spec, width) {
  const axes = spec.axes;
  const series = colored(spec.series);
  const format = formatter(spec);
  const size = Math.min(spec.size ?? 320, width);
  const center = size / 2;
  const radius = center - 48;
  const rings = spec.rings ?? 4;
  const maxOf = (axis) => axis.max ?? spec.max ?? Math.max(...series.map((item) => item.values[axis.key] ?? 0));
  const angleOf = (i) => (i / axes.length) * TAU - Math.PI / 2;
  const pointOf = (i, share) => arcPoint(center, center, radius * share, angleOf(i));
  let marks = "";
  for (let ring = 1; ring <= rings; ring++) {
    marks += `<polygon class="grid-line" fill="none" points="${axes.map((_, i) => pointOf(i, ring / rings).join(",")).join(" ")}"/>`;
  }
  axes.forEach((axis, i) => {
    const [x, y] = pointOf(i, 1);
    const [lx, ly] = pointOf(i, 1 + 22 / radius);
    const anchor = Math.abs(lx - center) < 4 ? "middle" : lx > center ? "start" : "end";
    marks += `<line class="grid-line" x1="${center}" y1="${center}" x2="${x}" y2="${y}"/><text x="${lx}" y="${ly + 4}" text-anchor="${anchor}" style="fill: var(--secondary)">${esc(axis.label)}</text>`;
  });
  for (const item of series) {
    const points = axes.map((axis, i) => pointOf(i, Math.min(1, (item.values[axis.key] ?? 0) / (maxOf(axis) || 1))));
    marks += `<polygon class="mark fade" data-k="${esc(item.key)}" fill="color-mix(in oklab, ${item.color} 18%, transparent)" stroke="${item.color}" stroke-width="2" stroke-linejoin="round" points="${points.map((point) => point.join(",")).join(" ")}"/>`;
    points.forEach(([x, y], i) => {
      marks += `<circle class="mark" data-k="${esc(item.key)}" cx="${x}" cy="${y}" r="3.5" fill="${item.color}"${hoverAttributes({ key: item.key, tip: `${item.label}, ${axes[i].label}  ${withUnit(spec, format(item.values[axes[i].key] ?? 0))}` })}/>`;
    });
  }
  figure.insertAdjacentHTML("beforeend", `<span class="chart-label">${esc(spec.label)}</span>${svgOpen(size, size, spec.label)}${marks}</svg>`);
  if (series.length > 1 && spec.legend !== false) figure.insertAdjacentHTML("beforeend", legend(series));
}

function waffleChart(figure, spec, width) {
  const data = spec.data;
  const format = formatter(spec);
  const rows = spec.rows ?? 10;
  const columns = spec.columns ?? 10;
  const cells = rows * columns;
  const total = sum(data.map((datum) => datum.value));
  head(figure, spec, format(total), spec.period ?? "");
  // Largest remainder, so the cells add up to the grid exactly.
  const exact = data.map((datum) => (datum.value / total) * cells);
  const counts = exact.map(Math.floor);
  const byRemainder = exact.map((value, i) => ({ i, rest: value - counts[i] })).sort((a, b) => b.rest - a.rest);
  const missing = cells - sum(counts);
  for (let i = 0; i < missing; i++) counts[byRemainder[i % byRemainder.length].i]++;
  const cell = Math.min(22, (Math.min(width, 360) - 3 * (columns - 1)) / columns);
  const items = data.map((datum, i) => ({
    key: datum.key,
    label: datum.label,
    color: datum.key === spec.accentKey ? "var(--accent)" : colorOf(datum, i + (spec.accentKey ? 1 : 0)),
    value: percent(datum.value, total, spec.decimals ?? 0),
  }));
  let marks = "";
  let index = 0;
  items.forEach((item, category) => {
    for (let n = 0; n < counts[category]; n++, index++) {
      const x = (index % columns) * (cell + 3);
      const y = Math.floor(index / columns) * (cell + 3);
      marks += `<rect class="mark fade" style="animation-delay: ${index * 0.004}s" x="${x}" y="${y}" width="${cell}" height="${cell}" rx="3" fill="${item.color}"${hoverAttributes({ key: item.key, value: format(data[category].value), detail: `${item.label}, ${item.value}` })}/>`;
    }
  });
  const side = columns * (cell + 3) - 3;
  figure.insertAdjacentHTML("beforeend", `<div class="chart-row">${svgOpen(side, rows * (cell + 3) - 3, spec.label)}${marks}</svg>${spec.legend === false ? "" : legend(items, "chart-legend-column")}</div>`);
}

function slopeChart(figure, spec, width) {
  const data = spec.data;
  const format = formatter(spec);
  const changeOf = (item) => {
    if (spec.formatChange !== "value") {
      const change = item.start ? ((item.end - item.start) / Math.abs(item.start)) * 100 : 0;
      return `${change >= 0 ? "+" : ""}${change.toFixed(0)}%`;
    }
    return `${item.end - item.start >= 0 ? "+" : ""}${format(item.end - item.start)}`;
  };
  const height = spec.height ?? Math.max(220, data.length * 22 + 60);
  const top = 28;
  const all = data.flatMap((item) => [item.start, item.end]);
  const low = Math.min(...all);
  const high = Math.max(...all);
  const yOf = (value) => top + 14 + (1 - (value - low) / (high - low || 1)) * (height - top - 28);
  const labelRoom = Math.min(180, width * 0.3);
  const left = labelRoom;
  const right = width - labelRoom;
  const startYs = spread(data.map((item) => yOf(item.start)), 18, top + 10, height - 4);
  const endYs = spread(data.map((item) => yOf(item.end)), 18, top + 10, height - 4);
  let marks = `<text x="${left}" y="14" text-anchor="middle" style="fill: var(--secondary)">${esc(spec.startLabel)}</text><text x="${right}" y="14" text-anchor="middle" style="fill: var(--secondary)">${esc(spec.endLabel)}</text>`;
  marks += `<line class="grid-line" x1="${left}" x2="${left}" y1="${top}" y2="${height}"/><line class="grid-line" x1="${right}" x2="${right}" y1="${top}" y2="${height}"/>`;
  data.forEach((item, i) => {
    let color = item.end >= item.start ? "var(--accent)" : shade(2);
    if (spec.highlightKey) color = item.key === spec.highlightKey ? "var(--accent)" : shade(3);
    const attributes = hoverAttributes({ key: item.key, tip: `${item.label}  ${format(item.start)} to ${format(item.end)}, ${changeOf(item)}` });
    marks += `<g class="mark fade"${attributes}><line x1="${left}" y1="${yOf(item.start)}" x2="${right}" y2="${yOf(item.end)}" stroke="${color}" stroke-width="2"/><line x1="${left}" y1="${yOf(item.start)}" x2="${right}" y2="${yOf(item.end)}" stroke="transparent" stroke-width="12"/><circle cx="${left}" cy="${yOf(item.start)}" r="4" fill="${color}"/><circle cx="${right}" cy="${yOf(item.end)}" r="4" fill="${color}"/>`;
    marks += `<text x="${left - 12}" y="${startYs[i] + 4}" text-anchor="end"><tspan style="fill: var(--secondary)">${esc(item.label)}</tspan>  ${esc(format(item.start))}</text>`;
    marks += `<text x="${right + 12}" y="${endYs[i] + 4}">${esc(format(item.end))}  <tspan style="fill: ${item.end >= item.start ? "var(--success)" : "var(--danger)"}">${esc(changeOf(item))}</tspan></text></g>`;
  });
  figure.insertAdjacentHTML("beforeend", `<span class="chart-label">${esc(spec.label)}</span>${svgOpen(width, height, spec.label)}${marks}</svg>`);
}

function splitBar(figure, spec, width) {
  const segments = spec.segments.filter((segment) => segment.value > 0);
  const format = formatter(spec);
  const total = sum(segments.map((segment) => segment.value));
  head(figure, { ...spec, label: spec.title ?? spec.label }, format(total), spec.description ?? spec.totalLabel ?? "");
  const items = segments.map((segment, i) => ({ key: segment.id, label: segment.label, color: colorOf(segment, i), value: format(segment.value) }));
  const gap = 3;
  const usable = width - gap * (segments.length - 1);
  let x = 0;
  let marks = "";
  segments.forEach((segment, i) => {
    const segmentWidth = Math.max(2, (segment.value / total) * usable);
    const share = percent(segment.value, total);
    marks += `<rect class="mark fade" style="animation-delay: ${i * 0.06}s" x="${x}" y="0" width="${segmentWidth}" height="40" rx="6" fill="${items[i].color}"${hoverAttributes({ key: segment.id, value: items[i].value, detail: `${segment.label}, ${share}` })}/>`;
    if (segmentWidth > 56) marks += `<text x="${x + 10}" y="25" style="fill: var(--background); font-weight: 500; pointer-events: none">${esc(share)}</text>`;
    x += segmentWidth + gap;
  });
  figure.insertAdjacentHTML("beforeend", `${svgOpen(width, 40, spec.title ?? spec.label ?? "")}${marks}</svg>${spec.legend === false ? "" : legend(items)}`);
}

function streamgraph(figure, spec, width) {
  const data = spec.data;
  const series = colored(spec.series);
  const format = formatter(spec);
  const totals = data.map((datum) => sum(series.map((item) => datum.values[item.key])));
  head(figure, spec, spec.value ?? format(sum(totals)), spec.period ?? "");
  const height = spec.height ?? 240;
  const offset = spec.offset ?? "silhouette";
  // shortcut: "wiggle" draws as "silhouette", the d3 wiggle minimisation only matters for many layers.
  const bases = totals.map((total) => (offset === "zero" ? 0 : -total / 2));
  const low = Math.min(...bases);
  const high = Math.max(...bases.map((base, i) => base + totals[i]));
  const xOf = (i) => (i / Math.max(1, data.length - 1)) * width;
  const yOf = (value) => 10 + (1 - (value - low) / (high - low || 1)) * (height - 20);
  const floors = [...bases];
  let marks = "";
  series.forEach((item, s) => {
    const bottom = data.map((_, i) => [xOf(i), yOf(floors[i])]);
    data.forEach((datum, i) => (floors[i] += datum.values[item.key] ?? 0));
    const top = data.map((_, i) => [xOf(i), yOf(floors[i])]);
    const back = curvePath([...bottom].reverse()).replace("M", "L");
    marks += `<path class="mark fade" style="animation-delay: ${s * 0.06}s" data-k="${esc(item.key)}" fill="${item.color}" stroke="var(--background)" stroke-width="1" d="${curvePath(top)}${back}Z"/>`;
  });
  const slot = width / Math.max(1, data.length - 1);
  data.forEach((datum, i) => {
    const tip = series.map((item) => `${item.label}  ${withUnit(spec, format(datum.values[item.key] ?? 0))}`).join("\n");
    marks += hoverColumn(xOf(i), slot, height, { value: format(totals[i]), detail: datum.label, tip }, guide(xOf(i), 0, height));
  });
  const labels = xAxis(data.map((datum) => datum.axisLabel ?? datum.label), xOf, height + 18);
  figure.insertAdjacentHTML("beforeend", `${svgOpen(width, height + AXIS, spec.label)}${marks}${labels}</svg>${spec.legend === false ? "" : legend(series)}`);
}

function nodeValue(node) {
  return node.children?.length ? sum(node.children.map(nodeValue)) : (node.value ?? 0);
}

// Squarified treemap (Bruls, Huizing, van Wijk): rows of tiles whose worst aspect ratio stops improving.
function squarify(items, x, y, width, height) {
  const total = sum(items.map((item) => item.size));
  const scale = (width * height) / (total || 1);
  const queue = items.filter((item) => item.size > 0).map((item) => ({ ...item, area: item.size * scale }));
  const tiles = [];
  while (queue.length) {
    const side = Math.min(width, height);
    const row = [queue.shift()];
    const worst = (list) => {
      const area = sum(list.map((item) => item.area));
      let ratio = 0;
      for (const item of list) ratio = Math.max(ratio, (side * side * item.area) / (area * area), (area * area) / (side * side * item.area));
      return ratio;
    };
    while (queue.length && worst([...row, queue[0]]) <= worst(row)) row.push(queue.shift());
    const area = sum(row.map((item) => item.area));
    const thickness = area / side;
    let offset = 0;
    for (const item of row) {
      const length = item.area / thickness;
      if (width >= height) tiles.push({ ...item, x, y: y + offset, width: thickness, height: length });
      else tiles.push({ ...item, x: x + offset, y, width: length, height: thickness });
      offset += length;
    }
    if (width >= height) {
      x += thickness;
      width -= thickness;
    } else {
      y += thickness;
      height -= thickness;
    }
  }
  return tiles;
}

function treemap(figure, spec, width) {
  const format = formatter(spec);
  const root = spec.data;
  const total = nodeValue(root);
  head(figure, spec, format(total), root.label);
  const height = spec.height ?? 320;
  const colors = [];
  const walk = (node) => {
    if (node.children?.length) node.children.forEach(walk);
    else if (node.color !== undefined) colors.push(node.color);
  };
  walk(root);
  const [low, high] = spec.colorDomain ?? [Math.min(...colors), Math.max(...colors)];
  const fillOf = (node, branch) => {
    if (node.color === undefined) return shade(branch + 1);
    const share = Math.min(1, Math.max(0, (node.color - low) / (high - low || 1)));
    return `color-mix(in oklab, var(--foreground) ${Math.round(14 + share * 34)}%, var(--surface))`;
  };
  let marks = "";
  const branches = squarify((root.children ?? [root]).map((node) => ({ node, size: nodeValue(node) })).sort((a, b) => b.size - a.size), 0, 0, width, height);
  branches.forEach((tile, branch) => {
    const leaves = tile.node.children?.length ? tile.node.children : null;
    const header = 22;
    if (leaves && tile.height > header + 12) {
      marks += `<text x="${tile.x + 4}" y="${tile.y + 15}" style="fill: var(--secondary)">${esc(fit(tile.node.label, tile.width - 8))}</text>`;
      const inner = squarify(leaves.map((node) => ({ node, size: nodeValue(node) })).sort((a, b) => b.size - a.size), tile.x + 2, tile.y + header, tile.width - 4, tile.height - header - 2);
      for (const leaf of inner) marks += treemapTile(leaf, `${tile.node.label} / ${leaf.node.label}`, fillOf(leaf.node, branch), format, spec, total);
    } else {
      marks += treemapTile(tile, tile.node.label, fillOf(tile.node, branch), format, spec, total);
    }
  });
  figure.insertAdjacentHTML("beforeend", `${svgOpen(width, height, spec.label)}${marks}</svg>`);
}

function treemapTile(tile, path, fill, format, spec, total) {
  const value = nodeValue(tile.node);
  const colorNote = tile.node.color !== undefined && spec.colorLabel ? `\n${spec.colorLabel}  ${tile.node.color}` : "";
  let html = `<rect class="mark fade" x="${tile.x + 1}" y="${tile.y + 1}" width="${Math.max(0, tile.width - 2)}" height="${Math.max(0, tile.height - 2)}" rx="4" fill="${fill}"${hoverAttributes({ key: path, value: format(value), detail: path, tip: `${path}  ${withUnit(spec, format(value))}, ${percent(value, total)}${colorNote}` })}/>`;
  if (tile.width > 48 && tile.height > 34) {
    html += `<text x="${tile.x + 8}" y="${tile.y + 18}" style="fill: var(--foreground); pointer-events: none">${esc(fit(tile.node.label, tile.width - 16))}</text><text x="${tile.x + 8}" y="${tile.y + 33}" style="fill: var(--secondary); pointer-events: none">${esc(format(value))}</text>`;
  }
  return html;
}

function fit(text, room) {
  // 6.6 px: the average advance of a 12 px Inter or system sans character.
  const CHAR_WIDTH = 6.6;
  const chars = Math.floor(room / CHAR_WIDTH);
  return text.length <= chars ? text : `${text.slice(0, Math.max(1, chars - 1))}...`;
}

function ridgeline(figure, spec, width) {
  const series = spec.series;
  const format = formatter(spec);
  const all = series.flatMap((item) => item.values);
  const [low, high] = spec.domain ?? [Math.min(...all), Math.max(...all)];
  const rowHeight = spec.rowHeight ?? 34;
  const peak = rowHeight * (spec.overlap ?? 1.8);
  const labelRoom = Math.min(140, width * 0.28);
  const plotWidth = width - labelRoom;
  const height = series.length * rowHeight + peak - rowHeight + 8;
  const samples = 96;
  const densities = series.map((item) => {
    const n = item.values.length;
    const mean = sum(item.values) / n;
    const deviation = Math.sqrt(sum(item.values.map((value) => (value - mean) ** 2)) / n) || (high - low) / 10 || 1;
    const bandwidth = spec.bandwidth ?? 1.06 * deviation * n ** -0.2;
    const curve = [];
    for (let i = 0; i < samples; i++) {
      const x = low + (i / (samples - 1)) * (high - low);
      let density = 0;
      for (const value of item.values) density += Math.exp(-0.5 * ((x - value) / bandwidth) ** 2);
      curve.push(density);
    }
    return curve;
  });
  const top = Math.max(...densities.flat()) || 1;
  let marks = "";
  series.forEach((item, row) => {
    const base = peak + row * rowHeight;
    const points = densities[row].map((density, i) => [labelRoom + (i / (samples - 1)) * plotWidth, base - (density / top) * peak]);
    const sorted = [...item.values].sort((a, b) => a - b);
    const median = sorted[Math.floor(sorted.length / 2)];
    const tip = `${item.label}  median ${withUnit(spec, format(median))}, ${item.values.length} values`;
    marks += `<g class="mark fade" style="animation-delay: ${row * 0.05}s"${hoverAttributes({ key: item.id, tip })}><path fill="color-mix(in oklab, var(--foreground) ${spec.tint === false ? 18 : 14 + (row % 3) * 8}%, var(--background))" stroke="var(--accent)" stroke-width="1.25" d="${areaPath(curvePath(points), labelRoom, labelRoom + plotWidth, base)}"/>`;
    marks += `<text x="${labelRoom - 12}" y="${base - 4}" text-anchor="end" style="fill: var(--secondary)">${esc(item.label)}</text></g>`;
  });
  const ticks = niceScale(low, high).ticks.filter((tick) => tick >= low && tick <= high);
  for (const tick of ticks) marks += `<text x="${labelRoom + ((tick - low) / (high - low || 1)) * plotWidth}" y="${height + 16}" text-anchor="middle">${esc(compact.format(tick))}</text>`;
  figure.insertAdjacentHTML("beforeend", `<span class="chart-label">${esc(spec.label)}</span>${svgOpen(width, height + AXIS, spec.label)}${marks}</svg>`);
}

function activityHeatmap(figure, spec, width) {
  const day = 86_400_000;
  const days = spec.days.map((entry) => ({ time: Date.parse(entry.date), count: entry.count })).sort((a, b) => a.time - b.time);
  const unit = spec.unit ?? { one: "contribution", other: "contributions" };
  // A repeated date keeps its last count, as its cell shows.
  const counts = new Map(days.map((entry) => [entry.time, entry.count]));
  const total = sum(counts.values());
  head(figure, { label: spec.label }, `${total.toLocaleString("en-US")} ${total === 1 ? unit.one : unit.other}`, spec.period ?? "");
  const first = days[0].time - new Date(days[0].time).getUTCDay() * day;
  const weeks = Math.ceil((days[days.length - 1].time - first + day) / (7 * day));
  const left = 30;
  const cell = Math.max(6, Math.min(14, (width - left) / weeks - 3));
  const nonzero = [...counts.values()].filter((count) => count > 0).sort((a, b) => a - b);
  const quantile = (share) => nonzero[Math.min(nonzero.length - 1, Math.floor(share * nonzero.length))] ?? 1;
  const cuts = spec.thresholds ?? [quantile(0.25), quantile(0.5), quantile(0.75)];
  const levelOf = (count) => {
    if (count <= 0) return 0;
    const level = cuts.findIndex((cut) => count <= cut);
    return level === -1 ? 4 : level + 1;
  };
  const fills = ["var(--surface-muted)", ...[30, 46, 64, 84].map((mix) => `color-mix(in oklab, var(--foreground) ${mix}%, var(--surface))`)];
  const dateText = new Intl.DateTimeFormat("en-US", { month: "short", day: "numeric", year: "numeric", timeZone: "UTC" });
  const monthText = new Intl.DateTimeFormat("en-US", { month: "short", timeZone: "UTC" });
  let marks = "";
  let lastMonth = -1;
  let lastLabelWeek = -3;
  for (let week = 0; week < weeks; week++) {
    const weekStart = new Date(first + week * 7 * day);
    if (weekStart.getUTCMonth() !== lastMonth && week < weeks - 2 && week - lastLabelWeek >= 3) {
      lastMonth = weekStart.getUTCMonth();
      lastLabelWeek = week;
      marks += `<text x="${left + week * (cell + 3)}" y="10">${monthText.format(weekStart)}</text>`;
    }
    for (let weekday = 0; weekday < 7; weekday++) {
      const time = first + (week * 7 + weekday) * day;
      if (time < days[0].time || time > days[days.length - 1].time) continue;
      const count = counts.get(time) ?? 0;
      marks += `<rect x="${left + week * (cell + 3)}" y="${18 + weekday * (cell + 3)}" width="${cell}" height="${cell}" rx="3" fill="${fills[levelOf(count)]}"${hoverAttributes({ tip: `${count} ${count === 1 ? unit.one : unit.other} on ${dateText.format(time)}` })}/>`;
    }
  }
  for (const [weekday, name] of [[1, "Mon"], [3, "Wed"], [5, "Fri"]]) marks += `<text x="0" y="${18 + weekday * (cell + 3) + cell - 2}">${name}</text>`;
  const height = 18 + 7 * (cell + 3);
  let key = '<div class="chart-legend" style="align-items: center">Less';
  for (const fill of fills) key += `<i style="width: 10px; height: 10px; background: ${fill}"></i>`;
  figure.insertAdjacentHTML("beforeend", `${svgOpen(left + weeks * (cell + 3), height, spec.label)}${marks}</svg>${key}More</div>`);
}

function funnelChart(figure, spec, width) {
  const stages = spec.stages;
  const segments = spec.segments ?? [];
  const format = formatter(spec);
  const valueOf = (stage) => stage.value ?? sum(Object.values(stage.values ?? {}));
  const values = stages.map(valueOf);
  head(figure, spec, format(values[0]), spec.period ?? `${percent(values[values.length - 1], values[0])} convert end to end`);
  // shortcut: always horizontal, Arc turns the funnel vertical below 420 px; add that when phones read these pages.
  const height = spec.height ?? 220;
  const labelTop = 52;
  const thickness = height - labelTop;
  const slot = width / stages.length;
  const half = (value) => ((value / values[0]) * thickness) / 2;
  const middle = labelTop + thickness / 2;
  // An edge holds each stage flat around its centre and eases into the next, sampled every 3 px like Arc's.
  const edge = (ys) => {
    const points = [];
    for (let x = 0; x <= width; x += 3) {
      const position = Math.min(Math.max(x / slot - 0.5, 0), stages.length - 1);
      const i = Math.floor(position);
      const t = Math.min(1, Math.max(0, (position - i - 0.32) / 0.36));
      const eased = t * t * (3 - 2 * t);
      points.push([x, ys[i] + (ys[Math.min(i + 1, stages.length - 1)] - ys[i]) * eased]);
    }
    return points;
  };
  const band = (aboveShares, belowShares) => {
    const top = edge(values.map((value, i) => middle - half(value) + aboveShares[i] * half(value) * 2));
    const bottom = edge(values.map((value, i) => middle - half(value) + belowShares[i] * half(value) * 2)).reverse();
    return `M${[...top, ...bottom].map(([x, y]) => `${x} ${y.toFixed(2)}`).join("L")}Z`;
  };
  let marks = "";
  if (segments.length) {
    // Each segment is a layer of the band, stacked from the top in the stage's own proportions.
    let above = stages.map(() => 0);
    segments.forEach((segment, s) => {
      const below = above.map((share, i) => share + (stages[i].values?.[segment.key] ?? 0) / (values[i] || 1));
      marks += `<path class="mark fade" data-k="${esc(segment.key)}" fill="${shade(s)}" d="${band(above, below)}"/>`;
      above = below;
    });
  } else {
    marks += `<path class="fade" fill="color-mix(in oklab, var(--accent) 70%, var(--surface))" d="${band(stages.map(() => 0), stages.map(() => 1))}"/>`;
  }
  stages.forEach((stage, i) => {
    const conversion = i === 0 ? "" : `${percent(values[i], values[i - 1])} of previous`;
    const tip = segments.length ? segments.map((segment) => `${segment.label}  ${format(stage.values?.[segment.key] ?? 0)}`).join("\n") : undefined;
    marks += `<g${hoverAttributes({ value: format(values[i]), detail: `${stage.label}${conversion ? `, ${conversion}` : ""}`, tip })}><rect x="${i * slot}" y="0" width="${slot}" height="${height}" fill="transparent"/>`;
    marks += `<text x="${i * slot + 8}" y="14" style="fill: var(--secondary)">${esc(fit(stage.label, slot - 12))}</text><text x="${i * slot + 8}" y="36" style="font: 500 18px var(--display); fill: var(--foreground)">${esc(format(values[i]))}</text>`;
    if (i > 0) marks += `<line class="grid-line" x1="${i * slot}" x2="${i * slot}" y1="0" y2="${height}"/>`;
    marks += "</g>";
  });
  figure.insertAdjacentHTML("beforeend", `${svgOpen(width, height, spec.label)}${marks}</svg>${segments.length && spec.legend !== false ? legend(segments.map((segment, s) => ({ ...segment, color: shade(s) }))) : ""}`);
}

function sunburst(figure, spec, width) {
  const format = formatter(spec);
  const root = spec.data;
  const total = nodeValue(root);
  head(figure, spec, format(total), root.label);
  const depth = spec.depth ?? 3;
  const size = Math.min(spec.size ?? 320, width);
  const center = size / 2;
  const hole = size * 0.16;
  const ring = (center - 4 - hole) / depth;
  let marks = "";
  const place = (node, level, start, sweep, branch, path) => {
    let angle = start;
    for (const [index, child] of (node.children ?? []).entries()) {
      const share = nodeValue(child) / (nodeValue(node) || 1);
      const childSweep = sweep * share;
      const ownBranch = level === 0 ? index : branch;
      const childPath = [...path, child.label];
      if (childSweep > 0.004) {
        const fill = `color-mix(in oklab, ${shade(ownBranch)}, var(--background) ${level * 16}%)`;
        const tip = `${childPath.join(" / ")}  ${withUnit(spec, format(nodeValue(child)))}, ${percent(nodeValue(child), total)}`;
        marks += `<path class="mark fade" style="animation-delay: ${level * 0.08}s" fill="${fill}" stroke="var(--background)" stroke-width="1.5" d="${ringPath(center, center, hole + level * ring, hole + (level + 1) * ring, angle, angle + childSweep)}"${hoverAttributes({ key: childPath.join("/"), value: format(nodeValue(child)), detail: childPath.join(" / "), tip })}/>`;
      }
      if (level + 1 < depth) place(child, level + 1, angle, childSweep, ownBranch, childPath);
      angle += childSweep;
    }
  };
  place(root, 0, -Math.PI / 2, TAU, 0, []);
  const branches = (root.children ?? []).map((child, i) => ({ key: child.label, label: child.label, color: shade(i), value: percent(nodeValue(child), total, 0) }));
  figure.insertAdjacentHTML("beforeend", `<div class="chart-row">${svgOpen(size, size, spec.label)}${marks}</svg>${spec.legend === false ? "" : legend(branches, "chart-legend-column")}</div>`);
}

function raceBarChart(figure, spec, width) {
  const items = new Map(spec.items.map((item) => [item.id, item]));
  const groups = (spec.groups ?? []).map((group) => group.id);
  const format = formatter(spec);
  const shown = spec.bars ?? 10;
  const row = 36;
  head(figure, { label: spec.label }, spec.frames[0].period, "");
  figure.insertAdjacentHTML(
    "beforeend",
    `<div class="chart-race" style="height: ${shown * row}px"></div><div class="chart-controls"><button type="button">Play</button><input type="range" min="0" max="${spec.frames.length - 1}" value="0" aria-label="Period"></div>`,
  );
  const lane = figure.querySelector(".chart-race");
  const button = figure.querySelector(".chart-controls button");
  const slider = figure.querySelector(".chart-controls input");
  const rows = new Map();
  const colorOfItem = (item, index) => (item.id === spec.highlight ? "var(--accent)" : shade(item.group ? groups.indexOf(item.group) + 1 : index + 1));
  let index = 0;
  [...items.values()].forEach((item, i) => {
    const element = document.createElement("div");
    element.className = "chart-race-row";
    element.innerHTML = `<span class="chart-race-label">${esc(item.label)}</span><span class="chart-race-track"><i style="background: ${colorOfItem(item, i)}"></i><b></b></span>`;
    lane.append(element);
    rows.set(item.id, element);
  });
  const show = (frameIndex) => {
    index = frameIndex;
    const frame = spec.frames[frameIndex];
    const ranked = Object.entries(frame.values).sort((a, b) => b[1] - a[1]);
    const top = ranked[0]?.[1] || 1;
    for (const [id, element] of rows) {
      const rank = ranked.findIndex(([key]) => key === String(id));
      const value = frame.values[id] ?? 0;
      element.style.transform = `translateY(${(rank < 0 ? shown : rank) * row}px)`;
      element.style.opacity = rank >= 0 && rank < shown ? "1" : "0";
      element.querySelector("i").style.width = `${(value / top) * 100}%`;
      element.querySelector("b").textContent = format(value);
    }
    const headline = figure.querySelector(".chart-value > span");
    headline.textContent = frame.period;
    // showHeadline restores dataset.rest after a hover, so it must follow the period.
    headline.dataset.rest = frame.period;
    slider.value = String(frameIndex);
  };
  let timer;
  const pause = () => {
    clearInterval(timer);
    timer = undefined;
    button.textContent = "Play";
  };
  const play = () => {
    if (index >= spec.frames.length - 1) show(0);
    button.textContent = "Pause";
    timer = setInterval(() => {
      if (index < spec.frames.length - 1) return show(index + 1);
      if (spec.loop) return show(0);
      pause();
    }, spec.frameDuration ?? 1400);
  };
  button.addEventListener("click", () => (timer ? pause() : play()));
  slider.addEventListener("input", () => {
    pause();
    show(Number(slider.value));
  });
  show(0);
  if (spec.autoPlay && !matchMedia("(prefers-reduced-motion: reduce)").matches) play();
  return pause;
}

function timeOf(date) {
  return typeof date === "number" ? date : Date.parse(date);
}

function brushChart(figure, spec, width) {
  const points = spec.data.map((datum) => ({ t: timeOf(datum.date), value: datum.value })).sort((a, b) => a.t - b.t);
  const format = formatter(spec);
  const dateText = new Intl.DateTimeFormat("en-US", { month: "short", day: "numeric", year: "numeric", timeZone: "UTC" });
  const shortDate = new Intl.DateTimeFormat("en-US", { month: "short", day: "numeric", timeZone: "UTC" });
  head(figure, spec, format(points[points.length - 1].value), "");
  const height = spec.height ?? 220;
  const overview = spec.overviewHeight ?? 48;
  const plotWidth = width - GUTTER;
  const first = points[0].t;
  const last = points[points.length - 1].t;
  let range = spec.defaultRange?.map(timeOf) ?? [first + (last - first) * 0.6, last];
  const scale = niceScale(Math.min(...points.map((point) => point.value)), Math.max(...points.map((point) => point.value)));
  figure.insertAdjacentHTML("beforeend", `<div class="chart-brush-main"></div>${svgOpen(plotWidth, overview, `${spec.label} overview`)}</svg>`);
  const main = figure.querySelector(".chart-brush-main");
  const detail = figure.querySelector(".chart-detail");
  const strip = figure.lastElementChild;
  const stripX = (t) => ((t - first) / (last - first || 1)) * plotWidth;
  const stripY = (value) => 4 + (1 - (value - scale.from) / (scale.to - scale.from)) * (overview - 8);
  const stripLine = points.map((point) => [stripX(point.t), stripY(point.value)]);
  const drawMain = () => {
    const visible = points.filter((point) => point.t >= range[0] && point.t <= range[1]);
    const axis = yAxis(scale, plotWidth, 12, height - 12);
    const xOf = (t) => ((t - range[0]) / (range[1] - range[0] || 1)) * plotWidth;
    const line = visible.map((point) => [xOf(point.t), axis.y(point.value)]);
    const path = curvePath(line);
    let marks = axis.html;
    if (line.length) marks += `<path fill="color-mix(in oklab, var(--accent) 14%, transparent)" d="${areaPath(path, line[0][0], line[line.length - 1][0], axis.y(Math.max(0, scale.from)))}"/><path fill="none" stroke="var(--accent)" stroke-width="2" d="${path}"/>`;
    const slot = plotWidth / Math.max(1, visible.length);
    for (const point of visible) {
      marks += hoverColumn(xOf(point.t), slot, height, { value: format(point.value), detail: dateText.format(point.t) }, `${guide(xOf(point.t), 12, height)}${dot(xOf(point.t), axis.y(point.value), "var(--accent)")}`);
    }
    // After the columns, so an annotation's own tip wins over the column under it.
    for (const note of spec.annotations ?? []) {
      const t = timeOf(note.date);
      if (t < range[0] || t > range[1]) continue;
      marks += `<g${hoverAttributes({ tip: `${note.label}${note.description ? `\n${note.description}` : ""}` })}><line x1="${xOf(t)}" x2="${xOf(t)}" y1="12" y2="${height}" stroke="var(--secondary)" stroke-dasharray="3 3"/><circle cx="${xOf(t)}" cy="12" r="5" fill="var(--surface-muted)" stroke="var(--secondary)"/></g>`;
    }
    const ticks = [range[0], (range[0] + range[1]) / 2, range[1]];
    marks += xAxis(ticks.map((t) => shortDate.format(t)), (i) => xOf(ticks[i]), height + 18);
    main.innerHTML = `${svgOpen(width, height + AXIS, spec.label)}${marks}</svg>`;
    detail.textContent = `${dateText.format(range[0])} to ${dateText.format(range[1])}`;
    detail.dataset.rest = detail.textContent;
    const x0 = stripX(range[0]);
    const x1 = stripX(range[1]);
    strip.innerHTML = `<path fill="none" stroke="var(--secondary)" stroke-width="1.25" d="${curvePath(stripLine)}"/><rect x="0" y="0" width="${x0}" height="${overview}" fill="var(--background)" opacity=".6"/><rect x="${x1}" y="0" width="${plotWidth - x1}" height="${overview}" fill="var(--background)" opacity=".6"/><rect x="${x0}" y=".5" width="${x1 - x0}" height="${overview - 1}" rx="6" fill="color-mix(in oklab, var(--accent) 10%, transparent)" stroke="var(--border-strong)" style="cursor: grab"/><rect x="${x0 - 3}" y="${overview / 2 - 10}" width="6" height="20" rx="3" fill="var(--accent)" style="cursor: ew-resize"/><rect x="${x1 - 3}" y="${overview / 2 - 10}" width="6" height="20" rx="3" fill="var(--accent)" style="cursor: ew-resize"/>`;
  };
  // Dragging near an edge resizes the window, inside it pans, outside it draws a new one.
  strip.style.touchAction = "none";
  strip.addEventListener("pointerdown", (event) => {
    const box = strip.getBoundingClientRect();
    const tAt = (clientX) => first + (Math.min(Math.max(clientX - box.left, 0), plotWidth) / plotWidth) * (last - first);
    const startT = tAt(event.clientX);
    const from = [...range];
    const near = (t) => Math.abs(stripX(t) - (event.clientX - box.left)) < 10;
    const mode = near(from[0]) ? "start" : near(from[1]) ? "end" : startT > from[0] && startT < from[1] ? "pan" : "new";
    const minSpan = spec.minSpan ?? (last - first) / 50;
    strip.setPointerCapture(event.pointerId);
    let frame;
    const move = (moveEvent) => {
      const t = tAt(moveEvent.clientX);
      if (mode === "start") range = [Math.min(t, from[1] - minSpan), from[1]];
      if (mode === "end") range = [from[0], Math.max(t, from[0] + minSpan)];
      if (mode === "new") range = [Math.min(startT, t), Math.max(startT, t, Math.min(startT, t) + minSpan)];
      if (mode === "pan") {
        const shift = Math.min(Math.max(t - startT, first - from[0]), last - from[1]);
        range = [from[0] + shift, from[1] + shift];
      }
      // Pointer events can outrun the display; one redraw per frame.
      frame ??= requestAnimationFrame(() => {
        frame = undefined;
        drawMain();
      });
    };
    strip.addEventListener("pointermove", move);
    strip.addEventListener("pointerup", () => strip.removeEventListener("pointermove", move), { once: true });
  });
  drawMain();
}

function realtimeStream(figure, spec, width) {
  const all = spec.points.map((point) => ({ ...point, t: timeOf(point.t) })).sort((a, b) => a.t - b.t);
  const end = all[all.length - 1].t;
  const points = spec.windowMs ? all.filter((point) => point.t >= end - spec.windowMs) : all;
  const format = formatter(spec);
  const time = new Intl.DateTimeFormat("en-US", { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });
  head(figure, spec, format(points[points.length - 1].value), `${time.format(points[0].t)} to ${time.format(end)}`);
  const height = spec.height ?? 200;
  const plotWidth = width - GUTTER;
  const values = points.map((point) => point.value);
  const scale = niceScale(spec.min ?? Math.min(...values), spec.max ?? Math.max(...values));
  const axis = yAxis(scale, plotWidth, 14, height - 14);
  const xOf = (t) => ((t - points[0].t) / (end - points[0].t || 1)) * plotWidth;
  const line = points.map((point) => [xOf(point.t), axis.y(point.value)]);
  const path = curvePath(line);
  let marks = `${axis.html}<path class="fade" fill="color-mix(in oklab, var(--accent) 12%, transparent)" d="${areaPath(path, 0, plotWidth, axis.y(Math.max(0, scale.from)))}"/><path class="draw" pathLength="1" fill="none" stroke="var(--accent)" stroke-width="1.75" d="${path}"/>`;
  const slot = plotWidth / Math.max(1, points.length - 1);
  for (const point of points) {
    const anomaly = point.anomaly ? `${typeof point.anomaly === "string" ? point.anomaly : "Anomaly"}\n` : "";
    const [x, y] = [xOf(point.t), axis.y(point.value)];
    if (point.anomaly) marks += dot(x, y, "var(--danger)", 5);
    const hover = { value: format(point.value), detail: time.format(point.t), tip: anomaly ? `${anomaly}${withUnit(spec, format(point.value))} at ${time.format(point.t)}` : undefined };
    marks += hoverColumn(x, slot, height, hover, `${guide(x, 14, height)}${dot(x, y, "var(--accent)")}`);
  }
  const [lx, ly] = line[line.length - 1];
  marks += `<circle cx="${lx}" cy="${ly}" r="4" fill="var(--accent)"/>`;
  const ticks = [points[0].t, (points[0].t + end) / 2, end];
  marks += xAxis(ticks.map((t) => time.format(t)), (i) => xOf(ticks[i]), height + 18);
  figure.insertAdjacentHTML("beforeend", `${svgOpen(width, height + AXIS, spec.label)}${marks}</svg>`);
}

function sankeyFlow(figure, spec, width) {
  const format = formatter(spec);
  const nodes = new Map(spec.nodes.map((node, i) => [node.id, { ...node, index: i, incoming: [], outgoing: [] }]));
  for (const link of spec.links) {
    for (const id of [link.source, link.target]) if (!nodes.has(id)) throw new Error(`link to unknown node ${id}`);
    nodes.get(link.source).outgoing.push(link);
    nodes.get(link.target).incoming.push(link);
  }
  // A node without a column sits one step right of its furthest source.
  const columnOf = (node, seen = new Set()) => {
    if (node.column !== undefined) return node.column;
    if (seen.has(node.id)) return 0;
    seen.add(node.id);
    node.column = node.incoming.length ? Math.max(...node.incoming.map((link) => columnOf(nodes.get(link.source), seen) + 1)) : 0;
    return node.column;
  };
  for (const node of nodes.values()) {
    columnOf(node);
    node.value = Math.max(sum(node.incoming.map((link) => link.value)), sum(node.outgoing.map((link) => link.value)));
  }
  const columnCount = Math.max(...[...nodes.values()].map((node) => node.column)) + 1;
  const columns = Array.from({ length: columnCount }, () => []);
  for (const node of nodes.values()) columns[node.column].push(node);
  const total = sum(columns[0].map((node) => node.value));
  head(figure, spec, format(total), spec.totalLabel ?? "");
  const height = spec.height ?? 320;
  const top = spec.columns ? 22 : 0;
  const nodeWidth = 10;
  const gap = 14;
  const scale = Math.min(...columns.map((column) => (height - top - gap * (column.length - 1)) / (sum(column.map((node) => node.value)) || 1)));
  const labelRoom = Math.min(120, width * 0.2);
  const xOf = (column) => (column / Math.max(1, columnCount - 1)) * (width - nodeWidth - labelRoom * (columnCount > 1 ? 1 : 0));
  let marks = "";
  columns.forEach((column, c) => {
    const used = sum(column.map((node) => node.value)) * scale + gap * (column.length - 1);
    let y = top + (height - top - used) / 2;
    for (const node of column) {
      node.x = xOf(c);
      node.y = y;
      node.height = Math.max(2, node.value * scale);
      node.outY = y;
      node.inY = y;
      y += node.height + gap;
    }
    if (spec.columns?.[c]) marks += `<text x="${xOf(c)}" y="12" style="fill: var(--secondary)">${esc(spec.columns[c])}</text>`;
  });
  const links = [...spec.links].sort((a, b) => nodes.get(a.source).y - nodes.get(b.source).y || nodes.get(a.target).y - nodes.get(b.target).y);
  for (const link of links) {
    const source = nodes.get(link.source);
    const target = nodes.get(link.target);
    const thickness = link.value * scale;
    const x0 = source.x + nodeWidth;
    const x1 = target.x;
    const middle = (x0 + x1) / 2;
    const d = `M${x0} ${source.outY}C${middle} ${source.outY} ${middle} ${target.inY} ${x1} ${target.inY}L${x1} ${target.inY + thickness}C${middle} ${target.inY + thickness} ${middle} ${source.outY + thickness} ${x0} ${source.outY + thickness}Z`;
    const tip = `${source.label} to ${target.label}  ${withUnit(spec, format(link.value))}, ${percent(link.value, source.value || 1)} of ${source.label}`;
    marks += `<path class="mark fade" fill="color-mix(in oklab, ${shade(source.index)} 38%, transparent)" d="${d}"${hoverAttributes({ key: `${link.source}>${link.target}`, value: format(link.value), detail: `${source.label} to ${target.label}`, tip })}/>`;
    source.outY += thickness;
    target.inY += thickness;
  }
  for (const node of nodes.values()) {
    marks += `<rect x="${node.x}" y="${node.y}" width="${nodeWidth}" height="${node.height}" rx="3" fill="${shade(node.index)}"${hoverAttributes({ value: format(node.value), detail: node.label })}/>`;
    const last = node.column === columnCount - 1 && columnCount > 1;
    marks += `<text x="${last ? node.x - 8 : node.x + nodeWidth + 8}" y="${node.y + node.height / 2 + 4}" text-anchor="${last ? "end" : "start"}" style="fill: var(--foreground)">${esc(node.label)} <tspan style="fill: var(--muted)">${esc(compact.format(node.value))}</tspan></text>`;
  }
  figure.insertAdjacentHTML("beforeend", `${svgOpen(width, height, spec.label)}${marks}</svg>`);
}
})();
