# Onboarding Wireframe 5-Step Structure Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Update `docs/wireframes/index.html` so the onboarding portion follows the desktop app's 5-step structure while keeping the existing wireframe deck styling and multi-source framing.

**Architecture:** Keep the work isolated to the onboarding slice inside the single static wireframe file. Reuse the deck's existing HTML, CSS utilities, and nav behavior; insert one new onboarding section, renumber the existing onboarding sections, and remove the sync-mode block so the local-folder step matches the current app's setup rhythm.

**Tech Stack:** Static HTML, inline CSS, existing vanilla JS screen navigation, local preview via `python3 -m http.server`

---

## File Map

- Modify: `docs/wireframes/index.html`
  Responsibility: onboarding nav entries, onboarding section order, step labels, step indicators, and onboarding copy/notes.
- Test: no automated test file for this static deck
  Validation: `rg`, `git diff --check`, and manual reload of `http://localhost:5500/`

### Task 1: Expand the onboarding skeleton from 4 steps to 5

**Files:**
- Modify: `docs/wireframes/index.html:268-323`
- Test: `docs/wireframes/index.html`

- [ ] **Step 1: Replace the onboarding nav entries with the 5-step sequence**

```html
    <div class="ngroup">Onboarding</div>
    <button data-s="ob1" aria-current="true"><span class="k">1.1</span>Welcome</button>
    <button data-s="ob2"><span class="k">1.2</span>How agents use it</button>
    <button data-s="ob3"><span class="k">1.3</span>Connect a source</button>
    <button data-s="ob4"><span class="k">1.4</span>Local folder</button>
    <button data-s="ob5"><span class="k">1.5</span>Ready</button>
```

- [ ] **Step 2: Update the welcome screen to show step 1 of 5 and a 5-dot indicator**

```html
    <!-- ============ 1.1 WELCOME ============ -->
    <section class="screen active" id="ob1">
      <div class="shead"><h2>Onboarding · Welcome</h2><span class="route">step 1 / 5</span></div>
      <p class="sdesc">Sets expectations and states the trust promise in one line. One primary action, one recovery path.</p>
      <div class="frame">
        <div class="tbar"><div class="dots"><i></i><i></i><i></i></div><span class="ttl">Locality Setup</span>
          <div class="steps"><i class="on"></i><i></i><i></i><i></i><i></i></div></div>
        <div style="padding:60px 40px;text-align:center">
          <img class="welcome-logo" src="assets/locality-logo-dark.svg" alt="Locality">
          <div style="font-size:20px;font-weight:600;letter-spacing:-.02em;max-width:420px;margin:0 auto">
            Turn your Notion and other docs into local files your AI agents can edit — safely.</div>
          <p style="color:var(--muted);max-width:400px;margin:12px auto 26px">
            Your edits stay on your machine until you review them in Review Center and push them back.</p>
          <span class="btn btn-primary">Get started →</span>
          <div style="margin-top:16px"><span class="linkish">Restore from an existing mount</span></div>
        </div>
      </div>
      <div class="notes"><b>notes</b> — traffic-light chrome + 5-dot step indicator persist across all setup steps ·
      "safely" is the thesis word: repeated in review-mode copy · secondary link handles reinstall/repair users.</div>
    </section>
```

- [ ] **Step 3: Verify the nav now exposes five onboarding entries**

Run: `rg -c 'data-s="ob[1-5]"' docs/wireframes/index.html`  
Expected:

```text
5
```

- [ ] **Step 4: Verify the welcome screen now uses step 1 of 5**

Run: `rg -n 'Onboarding · Welcome|step 1 / 5' docs/wireframes/index.html`  
Expected:

```text
307:      <div class="shead"><h2>Onboarding · Welcome</h2><span class="route">step 1 / 5</span></div>
```

- [ ] **Step 5: Commit the skeleton expansion**

```bash
git add docs/wireframes/index.html
git commit -m "docs: expand onboarding wireframe to 5 steps"
```

### Task 2: Insert the educational step and shift the connect screen to step 3

**Files:**
- Modify: `docs/wireframes/index.html:326-359`
- Test: `docs/wireframes/index.html`

- [ ] **Step 1: Replace the current `ob2` block with the dedicated “How agents use it” screen**

```html
    <!-- ============ 1.2 HOW AGENTS USE IT ============ -->
    <section class="screen" id="ob2">
      <div class="shead"><h2>Onboarding · How agents use it</h2><span class="route">step 2 / 5</span></div>
      <p class="sdesc">Teach the filesystem-first model before connection. Agents work in visible folders, Locality writes guidance, and changes stay local until review.</p>
      <div class="frame">
        <div class="tbar"><div class="dots"><i></i><i></i><i></i></div><span class="ttl">Locality Setup</span>
          <div class="steps"><i class="on"></i><i class="on"></i><i></i><i></i><i></i></div></div>
        <div style="padding:28px 36px 32px">
          <div style="font-size:17px;font-weight:600">Agents work in files you can see</div>
          <p style="color:var(--muted);margin:4px 0 18px">Each connected source appears as a local folder. Locality adds guidance files like AGENTS.md and CLAUDE.md so agents follow the filesystem contract, and edits stay local until you review them.</p>
          <div style="display:grid;grid-template-columns:1.05fr .95fr;gap:14px;max-width:620px">
            <div class="card" style="padding:14px">
              <div class="card-h">Visible local workspace</div>
              <div class="rowline"><span class="grow">notion-main/AGENTS.md</span><span class="chip c-synced">Guide</span></div>
              <div class="rowline"><span class="grow">Roadmap/page.md</span><span class="chip c-review">Edited</span></div>
              <div class="rowline"><span class="grow">Launch Plan/page.md</span><span class="chip c-synced">Synced</span></div>
            </div>
            <div class="card" style="padding:14px">
              <div style="font-weight:600;font-size:12.5px;margin-bottom:8px">What Locality does</div>
              <div class="check on"><i>✓</i><div><b style="font-size:12px">Keeps files visible</b><small>Agents and humans work in the same folder.</small></div></div>
              <div class="check on"><i>✓</i><div><b style="font-size:12px">Writes guidance</b><small>Agents see the filesystem contract inside the mount.</small></div></div>
              <div class="check on"><i>✓</i><div><b style="font-size:12px">Holds changes for review</b><small>Nothing syncs back until you approve it.</small></div></div>
            </div>
          </div>
          <div style="display:flex;justify-content:flex-end;gap:8px;margin-top:26px">
            <span class="btn">Back</span><span class="btn btn-primary">Continue →</span>
          </div>
        </div>
      </div>
      <div class="notes"><b>goal</b> — add the app-like educational pause without changing the deck's visual system ·
      this screen teaches the folder contract before any source authorization happens.</div>
    </section>
```

- [ ] **Step 2: Reintroduce the source-connection screen as `ob3` with step 3 of 5 and keep the multi-source cards**

```html
    <!-- ============ 1.3 CONNECT ============ -->
    <section class="screen" id="ob3">
      <div class="shead"><h2>Onboarding · Connect a source</h2><span class="route">step 3 / 5</span></div>
      <p class="sdesc">Connector cards scale as new sources ship. OAuth runs in the system browser; the step shows waiting, error, and connected states inline.</p>
      <div class="frame">
        <div class="tbar"><div class="dots"><i></i><i></i><i></i></div><span class="ttl">Locality Setup</span>
          <div class="steps"><i class="on"></i><i class="on"></i><i class="on"></i><i></i><i></i></div></div>
        <div style="padding:28px 36px 32px">
          <div style="font-size:17px;font-weight:600">Connect a source</div>
          <p style="color:var(--muted);margin:4px 0 18px">Locality only reads and writes the pages you authorize.</p>
          <div style="display:grid;grid-template-columns:1fr 1fr;gap:12px;max-width:560px">
            <div class="card" style="padding:14px">
              <div style="display:flex;gap:8px;align-items:center;font-weight:600"><span class="src src-n">N</span>Notion</div>
              <p style="color:var(--muted);font-size:11.5px;margin:6px 0 12px">Workspaces, pages, databases</p>
              <span class="btn btn-primary btn-sm">Connect</span>
            </div>
            <div class="card" style="padding:14px;opacity:.55">
              <div style="display:flex;gap:8px;align-items:center;font-weight:600"><span class="src src-g">G</span>Google Docs</div>
              <p style="color:var(--muted);font-size:11.5px;margin:6px 0 12px">Coming soon</p>
              <span class="btn btn-sm" aria-disabled="true">Connect</span>
            </div>
          </div>
          <div style="display:flex;gap:8px;align-items:center;margin-top:18px;color:var(--muted);font-size:12px">
            <span class="chip c-review">⟳ waiting</span> Waiting for authorization in your browser…
            <span class="btn btn-sm">Reopen browser</span><span class="linkish">Copy sign-in link</span>
          </div>
          <div style="display:flex;justify-content:flex-end;gap:8px;margin-top:26px">
            <span class="btn">Back</span><span class="btn btn-primary" style="opacity:.5">Continue →</span>
          </div>
        </div>
      </div>
      <div class="notes"><b>states</b> — error: “We couldn’t connect. [Try again]” · success: card shows workspace
      avatar + name + Synced-teal “Connected ✓”, Continue enables · copy-link is the no-browser fallback.</div>
    </section>
```

- [ ] **Step 3: Verify the new educational screen and the shifted connect step are both present**

Run: `rg -n 'How agents use it|Connect a source' docs/wireframes/index.html`  
Expected:

```text
270:    <button data-s="ob2"><span class="k">1.2</span>How agents use it</button>
327:      <div class="shead"><h2>Onboarding · How agents use it</h2><span class="route">step 2 / 5</span></div>
360:      <div class="shead"><h2>Onboarding · Connect a source</h2><span class="route">step 3 / 5</span></div>
```

- [ ] **Step 4: Verify the onboarding section ids now include the new `ob2` and shifted `ob3`**

Run: `rg -n 'id="ob[1-5]"' docs/wireframes/index.html | sed -n '1,5p'`  
Expected:

```text
306:    <section class="screen active" id="ob1">
327:    <section class="screen" id="ob2">
360:    <section class="screen" id="ob3">
```

- [ ] **Step 5: Commit the new educational step**

```bash
git add docs/wireframes/index.html
git commit -m "docs: add onboarding education step"
```

### Task 3: Realign the local-folder and ready screens to steps 4 and 5

**Files:**
- Modify: `docs/wireframes/index.html:361-424`
- Test: `docs/wireframes/index.html`

- [ ] **Step 1: Rewrite the local-folder section as `ob4` and remove the sync-mode choice**

```html
    <!-- ============ 1.4 FOLDER ============ -->
    <section class="screen" id="ob4">
      <div class="shead"><h2>Onboarding · Local folder</h2><span class="route">step 4 / 5</span></div>
      <p class="sdesc">Show where files appear and keep setup progress, approval waits, and recovery actions on this screen only.</p>
      <div class="frame">
        <div class="tbar"><div class="dots"><i></i><i></i><i></i></div><span class="ttl">Locality Setup</span>
          <div class="steps"><i class="on"></i><i class="on"></i><i class="on"></i><i class="on"></i><i></i></div></div>
        <div style="padding:26px 36px 30px;max-width:620px">
          <div style="font-size:17px;font-weight:600;margin-bottom:14px">Create your local folder</div>
          <div style="font-size:12px;color:var(--muted);margin-bottom:6px">Your connected source will appear here:</div>
          <div class="pathchip"><span>~/Library/CloudStorage/Locality/notion</span><em>Copy</em></div>
          <div style="margin:6px 0 18px"><span class="linkish">Choose a different location…</span></div>

          <div class="h-min">Verifying setup</div>
          <div class="card">
            <div class="rowline"><span class="chip c-synced">✓</span><span class="grow">Folder created</span></div>
            <div class="rowline"><span class="chip c-synced">✓</span><span class="grow">File Provider registered</span></div>
            <div class="rowline"><span class="chip c-review">⟳</span><span class="grow">Waiting for test file to appear…</span></div>
          </div>
          <div style="display:flex;justify-content:flex-end;gap:8px;margin-top:22px">
            <span class="btn">Back</span><span class="btn btn-primary">Continue →</span>
          </div>
          <div style="font-size:11px;color:var(--faint);margin-top:10px">Use this step for approval-required, waiting-for-folder, and retry-needed states.</div>
        </div>
      </div>
      <div class="notes"><b>error state</b> — a failed check swaps to ✕ Error red with a fix action:
      “File Provider not approved → [Open System Settings]” · keep all setup and recovery states on this screen.</div>
    </section>
```

- [ ] **Step 2: Move the ready screen to `ob5`, update it to step 5 of 5, and keep it as the handoff screen**

```html
    <!-- ============ 1.5 READY ============ -->
    <section class="screen" id="ob5">
      <div class="shead"><h2>Onboarding · Ready</h2><span class="route">step 5 / 5</span></div>
      <p class="sdesc">Completion handoff: mounted path, agent-ready promise, and next action into the product.</p>
      <div class="frame">
        <div class="tbar"><div class="dots"><i></i><i></i><i></i></div><span class="ttl">Locality Setup</span>
          <div class="steps"><i class="on"></i><i class="on"></i><i class="on"></i><i class="on"></i><i class="on"></i></div></div>
        <div style="padding:28px 36px 30px;max-width:620px">
          <div style="display:flex;align-items:center;gap:8px;font-size:17px;font-weight:600;margin-bottom:16px">
            <span class="chip c-synced" style="font-size:13px">✓</span> Locality is ready</div>
          <div style="font-size:12px;color:var(--muted);margin-bottom:6px">Your local workspace is mounted here:</div>
          <div class="pathchip"><span>~/Library/CloudStorage/Locality/notion</span><em>Copy</em></div>
          <div style="font-size:11.5px;color:var(--muted);margin:6px 0 16px">Agents can open this folder now, and Locality will hold their edits for review.</div>
          <div class="card" style="padding:12px 14px">
            <div style="font-weight:600;font-size:12.5px">⚡ Agent guidance installed</div>
            <div style="font-size:11.5px;color:var(--muted);margin:4px 0 10px">
              Claude Code · Cursor · Warp · Gemini CLI can learn the filesystem contract from guidance files inside the mount.</div>
            <span class="btn btn-sm">Copy sample prompt</span>
          </div>
          <div style="display:flex;justify-content:flex-end;gap:8px;margin-top:20px">
            <span class="btn">Open my folder</span><span class="btn btn-primary">Open Locality →</span>
          </div>
        </div>
      </div>
      <div class="notes"><b>handoff</b> — keep this screen about readiness, not setup progress ·
      the old education disclosure should not reappear here now that step 2 owns that job.</div>
    </section>
```

- [ ] **Step 3: Verify the sync-mode content has been removed from onboarding**

Run: `sed -n '305,455p' docs/wireframes/index.html | rg -n 'How should changes sync\?|Review mode|Live mode'`  
Expected: no output and exit code `1`

- [ ] **Step 4: Verify the folder and ready screens now use steps 4 and 5**

Run: `rg -n 'step 4 / 5|step 5 / 5' docs/wireframes/index.html`  
Expected:

```text
395:      <div class="shead"><h2>Onboarding · Local folder</h2><span class="route">step 4 / 5</span></div>
428:      <div class="shead"><h2>Onboarding · Ready</h2><span class="route">step 5 / 5</span></div>
```

- [ ] **Step 5: Commit the folder/ready realignment**

```bash
git add docs/wireframes/index.html
git commit -m "docs: realign onboarding folder and ready screens"
```

### Task 4: Polish onboarding references and verify the served deck

**Files:**
- Modify: `docs/wireframes/index.html:305-424`
- Test: `docs/wireframes/index.html`, local preview at `http://localhost:5500/`

- [ ] **Step 1: Make the onboarding comments and notes match the final 5-step sequence**

```html
    <!-- ============ 1.1 WELCOME ============ -->
    <!-- ============ 1.2 HOW AGENTS USE IT ============ -->
    <!-- ============ 1.3 CONNECT ============ -->
    <!-- ============ 1.4 FOLDER ============ -->
    <!-- ============ 1.5 READY ============ -->
```

- [ ] **Step 2: Run a whitespace and markup sanity check**

Run: `git diff --check`  
Expected: no output

- [ ] **Step 3: Verify all five onboarding screens are present in order**

Run: `rg -n '<button data-s="ob[1-5]"|<section class="screen( active)?\" id=\"ob[1-5]\"' docs/wireframes/index.html | sed -n '1,10p'`  
Expected:

```text
269:    <button data-s="ob1" aria-current="true"><span class="k">1.1</span>Welcome</button>
270:    <button data-s="ob2"><span class="k">1.2</span>How agents use it</button>
271:    <button data-s="ob3"><span class="k">1.3</span>Connect a source</button>
272:    <button data-s="ob4"><span class="k">1.4</span>Local folder</button>
273:    <button data-s="ob5"><span class="k">1.5</span>Ready</button>
306:    <section class="screen active" id="ob1">
327:    <section class="screen" id="ob2">
360:    <section class="screen" id="ob3">
393:    <section class="screen" id="ob4">
426:    <section class="screen" id="ob5">
```

- [ ] **Step 4: Reload the wireframe deck in the browser and click through onboarding**

Manual check at: `http://localhost:5500/`  
Expected:

```text
- The deck nav shows five onboarding entries.
- The second onboarding screen is "How agents use it".
- The connect screen still shows multiple source cards.
- The local-folder screen no longer shows sync-mode radios.
- The ready screen is the only completion handoff screen.
```

- [ ] **Step 5: Commit the final onboarding polish**

```bash
git add docs/wireframes/index.html
git commit -m "docs: polish onboarding wireframe flow"
```
