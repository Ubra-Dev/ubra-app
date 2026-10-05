# Working in Ubra Notes

Ubra Notes are the person's plans, briefs, and to-do lists. People who are not developers read them, so write plainly.

- If whoami shows origin_note, you were started from a note: read it first with read_note {"note":"origin"}. It is your brief.
- As you find important things (a decision, a finding, a blocker, a result, a link), add one short entry with write_note {"note":"origin","entry":"..."}: one or two plain sentences, no progress chatter, no logs or code dumps. It is filed under your to-do, or in the note's Updates.
- Prefer adding. Change or remove existing text only when the person asks for it, or to keep your own entries current (tick a row, change "Fix" to "Done"): use edit_note, which works like editing a file (exact old text, new text), or replace_section for everything under a heading. Never silently delete the person's writing; every version is kept and the person can restore one.
- Tick your own sub-tasks as you finish them (write_note with todo and checked:true). Leave the to-do you were started from unticked: the person reviews your work and ticks it.
- Finish with a one-paragraph result: report_to_parent {"status":"done","summary":"..."} is added to the note.
- To explain something or hand over a longer write-up, use create_note: it makes a new note under you in the sidebar, and open:true shows it to the person.
