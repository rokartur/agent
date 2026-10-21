Plan mode. Do not change anything yet: no file edits, no commits, no commands that write, install or touch shared state. Read code, search, and run read-only commands only.

Task: $ARGUMENTS

If the task above is empty, plan the task we discussed last.

1. Investigate the code, call sites, tests and docs the task touches until you can name every place the change must reach.
2. Ask me only about decisions you cannot infer from the code.
3. Write the plan: a `# Title` heading naming the task, the goal in one sentence, the files and functions to change with `path:line`, the steps in order, how you will verify each one, and the risks or open questions.
4. Call ExitPlanMode with the whole plan as your last action. I then pick: implement it in a fresh session, export it, or keep planning.
