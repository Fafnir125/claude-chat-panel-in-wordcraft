# Claude-Chat-Panel Implementierungsplan

> **Für Claude:** ERFORDERLICHE TEILFÄHIGKEIT: Diesen Plan Aufgabe für Aufgabe umsetzen.

**Ziel:** Ein schwebendes Chat-Panel unten rechts, über das Claude (via lokalem Claude Code, ohne API-Schlüssel) Fragen zum Dokument beantwortet und Änderungen vorschlägt, die erst nach „Übernehmen“ als ein Undo-Schritt angewendet werden.

**Architektur:** Die Engine (L4) bekommt `ai.*`-Befehle: `ai.read` (Absätze lesen), `ai.propose` (Vorschlag speichern, Dokument unverändert), `ai.apply` (Vorschlag als *ein* Befehl = ein Undo-Schritt anwenden, mit `rev`-Prüfung), `ai.discard`. Die UI (L6) startet `claude -p --output-format stream-json` mit einer MCP-Konfiguration, die auf `wordcraft-cli mcp --connect` zeigt; Claude darf nur `mcp__wordcraft__*` nutzen. Das Panel zeigt Kreis → Pille (Eingabe) → abgerundetes Quadrat (Antworten, untenbündig) und zeichnet Vorschau und Änderungsanimation als Overlay.

**Tech-Stack:** Rust, egui/eframe, serde_json, bestehender MCP-Server (`crates/mcp`), Claude Code CLI.

---

### Aufgabe 1: Engine – `cmd/ai.rs`

**Dateien:**
- Erstellen: `crates/engine/src/cmd/ai.rs`
- Ändern: `crates/engine/src/cmd.rs` (Modul + `specs()` registrieren), `crates/engine/src/session.rs` (Felder `ai_proposal: Option<AiProposal>`, `ai_marks: Vec<AiMark>`)
- Test: `crates/engine/src/cmd/ai.rs` (`#[cfg(test)] mod tests`)

**Befehle:**
- `ai.read {from?, to?}` – `pure`; `{rev, selection, paragraphs:[{block, style, text}]}`, höchstens 2000 Absätze.
- `ai.propose {edits:[{anchor:Pos, focus:Pos, text}], summary?}` – `pure`; validiert (höchstens 200 Edits, Text höchstens 100 000 Zeichen, Positionen geklemmt, keine Überlappung), speichert mit aktuellem `rev`.
- `ai.apply` – mutierend; Fehler, wenn kein Vorschlag oder `rev` abweicht („Dokument wurde seitdem geändert“). Edits rückwärts in Dokumentreihenfolge: Auswahl setzen + `text.insert {paragraphs:true}`. Schreibt `ai_marks` (neuer Bereich, alter Text) für die Animation.
- `ai.discard` – `pure`; verwirft den Vorschlag.

**Tests (zuerst rot, dann grün):**
1. `propose_does_not_change_document` – Text und Undo-Tiefe unverändert.
2. `apply_is_one_undo_step` – nach `ai.apply` mit 2 Edits stellt ein `edit.undo` den Ursprungstext wieder her.
3. `apply_refuses_after_user_edit` – `text.insert` zwischen propose und apply → Fehler, Dokument unverändert.
4. `apply_without_proposal_errors`.
5. `hostile_params_never_panic` (bestehend) bleibt grün.

Ausführen: `cargo test -p wordcraft-engine ai::` – erwartet: alle bestanden.

### Aufgabe 2: MCP – `get_selection`, Werkzeugbeschreibungen

**Dateien:** `crates/mcp/src/tools.rs`, `crates/mcp/src/tests.rs`
- Werkzeug `get_selection` → `execute_command ai.read` mit leerem Bereich (Auswahl + rev).
- Beschreibung von `execute_command` nennt den Ablauf `ai.read` → `ai.propose`.

Ausführen: `cargo test -p wordcraft-mcp` – erwartet: bestanden.

### Aufgabe 3: UI – Bridge (`crates/ui-egui/src/claude_chat/bridge.rs`)

- Nur `cfg(not(target_arch = "wasm32"))`.
- Startet `claude -p <prompt> --output-format stream-json --verbose --include-partial-messages --mcp-config <tmp.json> --allowedTools mcp__wordcraft__execute_command,mcp__wordcraft__get_selection [--resume <id>]` und hängt den Systemkontext mit `--append-system-prompt` an (Übersicht, Auswahl, Regel: Änderungen nur über `ai.propose`, auf Auswahl beschränken).
- Lese-Thread parst JSON-Zeilen → `ChatEvent::{Delta(String), Done{session_id}, Error(String)}` über `std::sync::mpsc`; `ctx.request_repaint()`.
- Fehlt `claude` → `Error("Claude Code wurde nicht gefunden …")`.
- Test: Parser-Unit-Test mit aufgezeichneten Beispielzeilen.

### Aufgabe 4: UI – Panel (`claude_chat/panel.rs`, `claude_chat/mod.rs`)

- Zustand: `Closed | Open`, `t_open` via `ctx.animate_bool_with_time_and_easing(id, open, 0.3, ease_out_cubic)`; reduzierte Bewegung → Dauer 0.
- Kreis 48 px, 16 px Rand unten rechts; Pille Breite 360 px (min. Fensterbreite − 32), Höhe wächst mit Zeilen bis 8, dann `ScrollArea`.
- Antwort-Quadrat: 360×360 (geklemmt), Radius 16, über der Pille, Nachrichten untenbündig (`stick_to_bottom`), „Neuer Chat“-Button.
- Vorschlag in der Antwort: „Übernehmen“ (`ai.apply`) / „Verwerfen“ (`ai.discard`).
- Tasten: Tab-Fokus, Enter senden, Shift+Enter Zeilenumbruch, Escape schließen; Klick außerhalb schließt.
- Bezug-Chip: aktuelle Auswahl (Kurzfassung 40 Zeichen), entfernbar.
- UI-Befehle `ui.claude.toggle` (Mod+Shift+K), `ui.claude.newChat` in `lib.rs`.
- Option `reduce_motion` in den Optionen.

### Aufgabe 5: UI – Overlay (`claude_chat/highlight.rs`)

- Auswahl-Bezug dauerhaft getönt, solange Panel offen.
- Vorschlag: alter Bereich gedämpft/durchgestrichen, neuer Text als Tönung.
- Nach `ai.apply`: `ai_marks` 1,2 s Aufleuchten, neuer Text Zeichen für Zeichen aufgedeckt (Overlay, nicht blockierend); reduzierte Bewegung → nur statische Tönung 1 s.

### Aufgabe 6: Verifizierung

- `cargo xtask ci` (fmt, clippy, tests, assets, layers, wasm) – Exit 0.
- `ui_shot`-Skripte: geschlossen, offen, mit Antwort, mit Vorschlag → PNGs lesen.
- Live-Test: App mit `--control 7981`, Frage stellen, Änderung vorschlagen, übernehmen, `edit.undo`.
- Checkliste der Akzeptanzkriterien Zeile für Zeile.
