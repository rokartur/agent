// Runs the harness around a Workflow script; `agent()` hands each call to the Rust loop in workflow.rs.
const __pending = new Map();
let __nextCall = 0;
let __phaseTitle = "";

globalThis.agent = (prompt, options) =>
  new Promise((resolve, reject) => {
    const id = __nextCall++;
    __pending.set(id, { resolve, reject });
    __request(id, String(prompt), JSON.stringify(options ?? {}), __phaseTitle);
  });

globalThis.__settle = (id, ok, text) => {
  const call = __pending.get(id);
  __pending.delete(id);
  if (ok) call.resolve(JSON.parse(text));
  else call.reject(new Error(text));
};

const __MAX_ITEMS = 4096;
const __checkSize = (name, list) => {
  if (!Array.isArray(list)) throw new TypeError(`${name}() takes an array`);
  if (list.length > __MAX_ITEMS) throw new RangeError(`${name}() takes at most ${__MAX_ITEMS} items, got ${list.length}`);
};
// A failed agent inside a fan-out leaves null in its slot instead of ending the run.
const __orNull = (start) =>
  Promise.resolve()
    .then(start)
    .catch((error) => {
      __log(`agent failed: ${error && error.message ? error.message : error}`);
      return null;
    });

globalThis.parallel = (tasks) => {
  __checkSize("parallel", tasks);
  return Promise.all(tasks.map((task) => __orNull(() => (typeof task === "function" ? task() : task))));
};

globalThis.pipeline = (items, ...stages) => {
  __checkSize("pipeline", items);
  if (stages.length === 0) throw new TypeError("pipeline() needs at least one stage");
  return Promise.all(
    items.map((item, index) =>
      __orNull(async () => {
        let value = item;
        for (const stage of stages) value = await stage(value, index);
        return value;
      }),
    ),
  );
};

globalThis.phase = (title) => {
  __phaseTitle = String(title);
  __phase(__phaseTitle);
};

globalThis.log = (...parts) => __log(parts.map((part) => (typeof part === "string" ? part : JSON.stringify(part))).join(" "));

const __nondeterministic = (what) => () => {
  throw new Error(`${what} is disabled in workflows, so a relaunched run repeats the same agent() calls. Pass a timestamp in through args.`);
};
Math.random = __nondeterministic("Math.random()");
const __Date = Date;
globalThis.Date = class extends __Date {
  constructor(...parts) {
    if (parts.length === 0) __nondeterministic("new Date()")();
    super(...parts);
  }
  static now = __nondeterministic("Date.now()");
};
