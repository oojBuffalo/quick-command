// Quick Command: type what you want in plain language; the shell command is pasted at the
// focused terminal's prompt. It is not run: you read it and press Enter yourself.
// The command comes from the `quick-command` program beside this file (see the README).
import { View, div } from "gpui-kit";
import { v_flex, h_flex, InputState, Input } from "gpui-base";
import { exec, run, props, closeOverlay, focused, theme, appHome, home } from "herdr";

const SPIN = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

export default class QuickCommand extends View {
  init(_props, cx) {
    // the terminal under the box when it opened: the command goes there and nowhere else
    const f = focused();
    this.terminal = f?.terminal ?? null;
    this.cwd = f?.cwd || home();
    this.busy = false;
    this.error = this.terminal ? "" : "No terminal is focused.";
    this.input = InputState.new({ placeholder: "Command instructions" });
    this.input.on("submit", (_e, cx) => this.submit(cx));
    this.input.focus();
    cx.timer.after(600, () => this.input.focus());
    this.handle = cx.focus_handle();
  }

  // Herdr calls this when the box goes away (Esc, a click beside it, or our own close).
  close() {
    this.closed = true;
  }

  fail(message, cx) {
    this.busy = false;
    this.error = message;
    cx.notify();
  }

  submit(cx) {
    const request = this.input.value().trim();
    if (!request || this.busy || !this.terminal) return;
    this.busy = true;
    this.error = "";
    cx.notify();
    cx.spawn(async (cx) => {
      let r;
      try {
        // shortcut: runs on this computer; a terminal on another machine gets a command made for this one.
        r = await exec(`${appHome()}/mods/quick-command/quick-command`, ["--once", request], { cwd: this.cwd, timeout: 30000 });
      } catch (e) {
        r = { code: 1, stdout: "", stderr: String(e?.message ?? e) };
      }
      if (this.closed) return; // closed while waiting: type nothing
      const command = r.stdout.trim();
      if (r.code !== 0 || !command) {
        const line = r.stderr.trim().split("\n").pop() || "no command came back";
        return this.fail(line.replace(/^error: /, ""), cx);
      }
      // The program already returns one clean line; this is the last check before it becomes keystrokes.
      if (/[\p{Cc}\p{Zl}\p{Zp}]/u.test(command)) return this.fail("the reply was not a single plain line", cx);
      const now = focused()?.terminal;
      if (now && now !== this.terminal) return this.fail("the focused terminal changed; nothing was typed", cx);
      // shortcut: goes through the clipboard (Herdr has no "type this without Enter" for mods),
      // so the command replaces what was copied. Restoring it could race the paste and put the
      // old clipboard at the prompt instead.
      cx.write_to_clipboard(command);
      closeOverlay(props().overlay);
      run("terminal.paste");
    });
  }

  render() {
    const t = theme();
    const status = this.busy
      ? h_flex().gap(6).text_color(t.accent).child(div().w(12).frames(SPIN, 90).child(SPIN[0])).child("generating…")
      : div().text_color(this.error ? t.ansi1 : t.dim).truncate().child(this.error || "↵ send · esc close · the command is pasted, not run");
    return v_flex().id("quick-command").track_focus(this.handle).w_full().min_h(0).p(14).gap(8).bg(t.popover).text_size(13)
      .on_key_down((e, cx) => {
        if (e.key !== "escape") return;
        closeOverlay(props().overlay);
        cx.stop_propagation();
      })
      .child(h_flex().h(36).flex_none().px(10).gap(8).items_center().rounded(9).border_1().border_color(t.accent).bg(t.background)
        .child(div().text_color(t.accent).child("❯"))
        .child(Input.new(this.input).flex_1().h_full().text_size(13).text_color(t.foreground)))
      .child(div().h(16).px(2).text_size(11.5).child(status));
  }
}
