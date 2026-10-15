//! B371 narrow production contracts. Expected baseline: two assertion failures.
//! M1: unresolved-final-set says terminal=no -> terminal_fact_survives_final_set_refusal.
//! M2: restore chars().take(n) -> rendering_keeps_complete_graphemes_within_columns.
//! M3: split graphemes with char-width accumulation -> same render contract.
//! M4: bypass clipping at render call sites -> documented source review plus render coverage.
//! Do not weaken seed before TaskRecorded. Full existing DSH/UI suites remain required.
use std::{path::Path, process::Command};
use orch_host::observation::ProjectObservation;
use orch_ui::app::AppState;

#[test]
fn terminal_fact_survives_final_set_refusal() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap()
        .join("orch/scripts/wake-dsh-stream.sh");
    // Execute the real production exit tail, using the B347 deterministic interleaving seam.
    let code = r#"
import pathlib,sys
text=pathlib.Path(sys.argv[1]).read_text()
start=text.index('captured_pending_write = False\n')
end=text.index('\nusage = terminal_record.get', start)
selection_error=None; envelope_mode=False; selected_id='fixture'; pin_verified=True
final_set_error='fixture unresolved candidate'; provider_rc=0; terminal_seen=True
record_frames=3; projected=0; timed_out=False
captured_pending_write=False
def diag(value): print(value, file=sys.stderr)
exec(compile(text[start:end], str(sys.argv[1]), 'exec'))
"#;
    let output = Command::new("python3").arg("-c").arg(code).arg(script).output().unwrap();
    assert_eq!(output.status.code(), Some(74), "refusal/exit precedence must remain intact");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("terminal=yes cause=unresolved-final-set rc=0 exit=74"), "{stderr}");
    assert!(!stderr.contains("terminal=no"), "a refused final set does not erase an observed terminal");
    assert!(output.stdout.is_empty(), "refusal must never publish a successful terminal");
}

fn snapshot(root: String, diagnostics: Vec<String>) -> ProjectObservation {
    ProjectObservation { root, read_at:"fixed".into(), rows:vec![], groups:vec![], diagnostics, truncated:false }
}
fn line(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
    use ratatui::buffer::CellWidth;
    if buffer.area.width==0 { return String::new(); }
    let row=&buffer.content[y as usize * buffer.area.width as usize..(y as usize+1)*buffer.area.width as usize];
    let mut x=0; let mut out=String::new();
    while x<row.len() { out.push_str(row[x].symbol()); x+=usize::from(row[x].cell_width()).max(1); }
    out
}
#[test]
fn rendering_keeps_complete_graphemes_within_columns() {
    // Combining clusters expose chars().take underfill even though Buffer bounds wide cells.
    let clusters="e\u{301}".repeat(8);
    let app=AppState::new(snapshot(String::new(), vec![clusters.clone()]));
    let buffer=app.render(8,6);
    assert_eq!(line(&buffer,2),clusters,"diagnostic/list line must fill eight display columns");
    let prefix="Fusion observations | Calls | ";
    let header=AppState::new(snapshot(clusters.clone(),vec![])).render((prefix.len()+8) as u16,4);
    assert_eq!(line(&header,0),format!("{prefix}{clusters}"),"header must retain complete clusters");
    let mut input=AppState::new(snapshot(String::new(),vec![]));
    input.project_input=Some(clusters.clone());
    assert_eq!(line(&input.render(17,6),2),format!("Project: {clusters}"));
    for (text,width,expected) in [("中文x",3,"中 "),("👩‍💻x",2,"👩‍💻"),("e\u{301}x",1,"e\u{301}")] {
        let b=AppState::new(snapshot(String::new(),vec![text.into()])).render(width,6);
        assert_eq!(line(&b,2),expected,"complete leading grapheme and display width {width}");
    }
    for width in [0,1,2,8] {
        let b=app.render(width,6); assert_eq!(b.area.width,width);
        if width>0 { assert!(ratatui::text::Span::raw(line(&b,5)).width()<=width as usize); }
    }
}
