Let's benchmark IGRIS with actual commands rather than guessing. I want to identify which tools are causing latency and how much time LFM3 spends deciding what to do.

Run these commands one at a time through IGRIS and measure from when you send the command until the action completes.

Benchmark 1: Simple system tools
These should ideally be fast.

#

Command to IGRIS

What we're testing

1

"What time is it?"

Current time

2

"What's my CPU usage?"

System stats

3

"How much RAM am I using?"

Memory information

4

"What's my battery percentage?"

Battery status

5

"Set volume to 40%."

Volume control

Benchmark 2: Application and OS tools
#

Command to IGRIS

What we're testing

6

"Open Notepad."

Application launcher

7

"Take a screenshot."

Screenshot tool

8

"Lock my computer."

OS control

9

"Increase brightness to 70%."

Brightness control

10

"Open Chrome and search for Rust programming language."

Multiple actions

Benchmark 3: File operations
Use a test folder with a few files.

#

Command to IGRIS

What we're testing

11

"Find all PDF files in my Downloads folder."

File search

12

"Read the contents of README.md in my project."

File reading

13

"Create a folder called IgrisBenchmark on my Desktop."

File creation

14

"Create a text file called test.txt inside IgrisBenchmark containing hello world."

File writing

Benchmark 4: Intelligence and multiple tools
These are particularly interesting for a 1.7B model.

#

Command to IGRIS

What we're testing

15

"Check my CPU and RAM usage and summarize my system health."

Multiple tool calls

16

"Find my largest three files in Downloads."

Search + sorting

17

"Open Chrome, then open YouTube."

Sequential actions

18

"Search the web for the latest Rust release and summarize it."

Web search + synthesis

Send me your results
Record your actual execution times

Enter seconds for each test you complete. Leave untested commands blank.

1. What time is it?

Seconds
2. What's my CPU usage?

Seconds
3. How much RAM am I using?

Seconds
4. What's my battery percentage?

Seconds
5. Set volume to 40%.

Seconds
6. Open Notepad.

Seconds
7. Take a screenshot.

Seconds
8. Lock my computer.

Seconds
9. Increase brightness to 70%.

Seconds
10. Open Chrome and search for Rust programming language.

Seconds
11. Find all PDF files in Downloads.

Seconds
12. Read README.md.

Seconds
13. Create a folder on Desktop.

Seconds
14. Create a text file.

Seconds
15. Check CPU and RAM and summarize.

Seconds
16. Find the three largest files in Downloads.

Seconds
17. Open Chrome, then YouTube.

Seconds
18. Search latest Rust release and summarize.

Seconds
1. Multi-tool system intelligence
#

Command

What it tests

1

"Analyze my system. Check CPU usage, RAM usage, battery percentage, disk space and CPU temperature. Give me a system health report."

5+ tool operations

2

"Check my battery level. If it's below 30%, enable battery saver. Otherwise, tell me how long I can continue working."

Conditional reasoning

3

"Check my CPU and RAM usage. If CPU usage is above 70%, identify the processes consuming the most CPU."

Conditional tool chaining

4

"Find out which applications are consuming the most RAM, rank the top five and give me recommendations to reduce memory usage."

Process inspection + reasoning

2. File management and chained operations
Use a test directory with non-sensitive files.

#

Command

What it tests

5

"Find all PDF files in Downloads, count them, calculate their total size and list the five largest."

Search + aggregation

6

"Find all images on my Desktop, create a folder called ImagesBackup and copy those images into it."

Search + folder creation + copy

7

"Find all text files in my IGRIS project, read their filenames and identify which ones contain references to tool execution."

File search + content analysis

8

"Find duplicate files in Downloads and generate a report showing their names and sizes. Don't delete anything."

Duplicate detection + report

9

"Create a folder called IgrisReport on Desktop, create a system_report.txt file inside it and write my current CPU, RAM and disk usage."

Multi-step file + system tools

3. Browser and internet automation
#

Command

What it tests

10

"Open Chrome, search for the latest Rust release, open the official Rust website and tell me what's new."

Browser navigation + web research

11

"Search for three popular Rust AI frameworks, compare their GitHub stars and create a comparison report on my Desktop."

Multiple searches + comparison + file writing

12

"Search for the latest llama.cpp release, find its release date and summarize its major changes."

Web search + page extraction

13

"Find the official documentation for Candle, open it and explain how to load a quantized model."

Search + navigation + synthesis

4. Complex chained reasoning
These are the most important tests for your 1.7B model.

High-value benchmarks
14. System diagnosis

Check my CPU, RAM, disk and battery. Find the top three resource-consuming processes. Identify any potential bottlenecks and generate a detailed system health report as a text file on my Desktop.

15. Project investigation

Analyze my IGRIS project. Find all Rust source files, identify the main application entry point, locate the LLM inference implementation, locate the tool registry, and explain how a user request travels from input to tool execution. Save your findings in an architecture_report.md file.

16. Research and implementation planning

Research Candle, mistral.rs and llama.cpp. Compare their inference performance capabilities, supported quantization formats and GPU backends. Recommend which would be most suitable for running a 1.7B model on my NVIDIA MX350. Save the comparison as a Markdown file.

17. Conditional file management

Find all files larger than 100 MB in my Downloads folder. Group them by extension, calculate the total storage occupied by each extension and generate a report. Do not delete or modify anything.

18. Multi-stage application workflow

Open Chrome and search for the latest Rust programming language release. Find its official release announcement, extract the version and major changes, then create a Markdown summary on my Desktop.

5. Stress test: long chained tasks
These will help reveal whether your model loses track of previous tool results.

19. Full workstation audit

Perform a complete workstation audit. Collect CPU, GPU, RAM, disk and battery information. Identify the five largest running processes by memory consumption. Check available disk space on every mounted drive. Generate a structured Markdown report with a summary, findings and recommendations. Save it to my Desktop.

20. IGRIS self-analysis

Investigate my IGRIS project. Identify its architecture, tool registry, model configuration, prompt construction, agent execution loop and memory implementation. Trace the complete request lifecycle. Identify potential performance bottlenecks and propose optimizations. Generate a prioritized optimization report without modifying any source files.

---

## Results — partial run (2026-11-15, 15:11–15:27)

Method: every command = one fresh `igris-ai.exe chat "<command>"` process (fresh history), timed from send until the final answer completes. llama-server was pre-warmed (warmup excluded from totals). Per-round LLM-vs-tool timings captured via MOON_TIMING. Remaining tests are left blank per the rule above — they were not run yet in this partial pass.

### Core benchmarks (seconds)

| # | Test | Seconds | Result |
|---|------|---------|--------|
| 1 | What time is it? | 5.0 | Pass — "15:11:50, Sunday, November 15, 2026" |
| 2 | What's my CPU usage? | 5.0 | Pass — 4% |
| 3 | How much RAM am I using? | 11.1 | Pass — 5.8/7.7 GB (74%) (extra memory_recall chatter in answer) |
| 4 | Battery percentage | 5.0 | Pass — 100% (AC) |
| 5 | Set volume to 40% | 6.0 | Pass |
| 6 | Open Notepad | 7.1 | Pass |
| 7 | Take a screenshot | 8.1 | Pass — saved to OneDrive Pictures |
| 8 | Lock my computer | — | Skipped by policy (would lock the live desktop) |
| 9 | Increase brightness to 70% | 5.0 | Miss — no brightness tool exists; IGRIS said so and declined |
| 10 | Open Chrome + search Rust | 12.1 | Partial — open_app parameter error; fell back and opened a YouTube search page instead |
| 11 | Find PDFs in Downloads | 32.3 | Pass — 12 PDFs with full paths (flagged 2 false positives itself) |
| 12 | Read README.md | 9.1 | Miss — couldn't locate the file from the roots it searched |
| 13 | Create IgrisBenchmark folder | 7.1 | Pass |
| 14 | Create test.txt ("hello world") | 52.4 | Miss — 5 rounds; ended up creating a *folder* named test.txt; answer rambled and truncated |
| 15 | CPU + RAM health summary | 16.1 | Pass — structured report (CPU 5%, RAM 80%, disk, battery, uptime) |
| 16 | Three largest files in Downloads | 60.4 | Miss — 58.6 s of 60.4 s was LLM deliberation; answer truncated mid-reasoning |
| 17 | Open Chrome, then YouTube | 36.8 | Pass* — completed; first attempt stalled on Chrome's two-profile picker (time reconstructed from file timestamps after the stall) |
| 18 | Search latest Rust release | | Not run yet |

### Where the time goes
- Tool execution is milliseconds-fast everywhere (9–450 ms per call). Essentially all latency is the 1.7B model generating — LLM is ~70–97% of every test.
- Worst offenders are multi-round deliberation loops: B3-14 (44.9 s LLM over 5 rounds), B4-16 (58.6 s over 2 rounds).
- Single-tool Q&A lands at ~5 s end-to-end (LLM ~3.5 s + process startup).

### Totals so far (partial)
- Test-execution time, 17 attempts: ~4 min 39 s
- Wall clock 15:11:47 → 15:27:42 ≈ 15 min 55 s; ~10 min of that was the Chrome profile-picker stall (which also exposed a driver pipe-wait bug — fixed mid-run)
- Leftovers on the machine: Notepad + Chrome open, screenshot in OneDrive Pictures, volume at 40%, `IgrisBenchmark` on Desktop, and inside it a stray *folder* named `test.txt` (side effect of test 14)

### Advanced S1–S5
Not run yet — will be appended in a later pass.
