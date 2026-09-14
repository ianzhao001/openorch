//! Real independent-bin PTY and byte-for-byte read-only navigation proof.
use std::process::Command;
const SCRIPT: &str = r#"
import os,sys,pty,termios,fcntl,struct,subprocess,select,time,json,tempfile,signal,pathlib
bin=sys.argv[1];results=[]
os.setsid();signal.signal(signal.SIGHUP,signal.SIG_IGN)
with tempfile.TemporaryDirectory(prefix='b353-pty-') as root:
 pathlib.Path(root,'sentinel').write_text('unchanged')
 before_tree={str(p.relative_to(root)):p.read_bytes() for p in pathlib.Path(root).rglob('*') if p.is_file()}
 cases={name:(name,ti,to) for name,ti,to in [('q',True,True),('ctrl-c',True,True),('input-resize',True,True),('stdout-pipe',True,False),('stdin-pipe',False,True),('both-pipe',False,False)]}
 for name,ti,to in [cases[sys.argv[2]]]:
  m,s=pty.openpty();before=termios.tcgetattr(s);fcntl.ioctl(s,termios.TIOCSWINSZ,struct.pack('HHHH',24,120,0,0))
  fcntl.ioctl(s,termios.TIOCSCTTY,0)
  p=subprocess.Popen([bin,'--root',root],stdin=s if ti else subprocess.DEVNULL,stdout=s if to else subprocess.PIPE,stderr=subprocess.PIPE,pass_fds=(s,))
  fds=[m,p.stderr.fileno()]+([] if to else [p.stdout.fileno()]);data=b'';raw=False;sent=False;end=time.monotonic()+12
  try:
   while time.monotonic()<end:
    for fd in select.select(fds,[],[],0.05)[0]:
     try:chunk=os.read(fd,65536)
     except OSError:chunk=b''
     if chunk:data+=chunk
     elif fd!=m:fds.remove(fd)
    flags=termios.tcgetattr(s)[3];raw_now=not(flags&termios.ICANON) and not(flags&termios.ECHO);raw|=raw_now
    if ti and to and raw_now and b'\x1b[?1049h' in data and not sent:
     if name=='input-resize':
      fcntl.ioctl(s,termios.TIOCSWINSZ,struct.pack('HHHH',8,18,0,0));os.kill(p.pid,signal.SIGWINCH);os.write(m,b'p');time.sleep(.1);os.write(m,'qyr中 '.encode());time.sleep(.1);assert p.poll() is None;os.write(m,b'\x1b');time.sleep(.1);os.write(m,b'q')
     else:os.write(m,b'q' if name=='q' else b'\x03')
     sent=True
    if p.poll() is not None:
     for fd in select.select(fds,[],[],.05)[0]:
      try:data+=os.read(fd,65536)
      except OSError:pass
     break
   else:raise AssertionError(name+' owned PTY deadline')
   raw_after=termios.tcgetattr(s);pending=getattr(termios,'PENDIN',0)
   assert (raw_after[3]^before[3])&~pending==0
   assert bool(raw_after[3]&termios.ICANON) and bool(raw_after[3]&termios.ECHO)
   fcntl.ioctl(s,termios.FIONREAD,struct.pack('i',0));after=termios.tcgetattr(s);record={'case':name,'before':repr(before),'raw_after':repr(raw_after),'after_query':repr(after),'exit':p.returncode,'raw_seen':raw,'termios_restored':after==before,'alternate_enter':b'\x1b[?1049h' in data,'alternate_leave':b'\x1b[?1049l' in data,'cursor_show':b'\x1b[?25h' in data,'bytes':len(data)};results.append(record)
   assert after==before,record
   if ti and to:assert p.returncode==0 and raw and record['alternate_leave'] and record['cursor_show'],record
   else:assert p.returncode!=0 and not raw and not record['alternate_enter'],record
  finally:
   if p.poll() is None:p.kill();p.wait()
   os.close(m);os.close(s);p.stderr.close()
   if p.stdout:p.stdout.close()
 assert before_tree=={str(p.relative_to(root)):p.read_bytes() for p in pathlib.Path(root).rglob('*') if p.is_file()}
print(json.dumps(results))
"#;
#[test]
fn actual_bin_tty_modes_quit_control_input_resize_and_readonly() {
    for name in [
        "q",
        "ctrl-c",
        "input-resize",
        "stdout-pipe",
        "stdin-pipe",
        "both-pipe",
    ] {
        let out = Command::new("python3")
            .args(["-c", SCRIPT, env!("CARGO_BIN_EXE_orch-tui"), name])
            .output()
            .unwrap();
        println!("{}", String::from_utf8_lossy(&out.stdout));
        assert!(
            out.status.success(),
            "{}: {}",
            name,
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
