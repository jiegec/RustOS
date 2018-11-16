use alloc::boxed::Box;
use alloc::sync::Arc;
use spin::Mutex;
use scheduler::Scheduler;
use core::cell::UnsafeCell;
use alloc::vec::Vec;
use event_hub::EventHub;

struct Process {
    id: Pid,
    status: Status,
    status_after_stop: Status,
    context: Option<Box<Context>>,
}

pub type Pid = usize;
type ExitCode = usize;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Status {
    Ready,
    Running(usize),
    Sleeping,
    Waiting(Pid),
    /// aka ZOMBIE. Its context was dropped.
    Exited(ExitCode),
}

#[derive(Eq, PartialEq)]
enum Event {
    Wakeup(Pid),
    Dropped,
}
pub trait Context {
    unsafe fn switch_to(&mut self, target: &mut Context);
}

pub struct ProcessManager {
    procs: Vec<Mutex<Option<Process>>>,
    scheduler: Mutex<Box<Scheduler>>,
    wait_queue: Vec<Mutex<Vec<Pid>>>,
    children: Vec<Mutex<Vec<Pid>>>,
    event_hub: Mutex<EventHub<Event>>,
    exit_handler: fn(Pid),
}

impl ProcessManager {
    pub fn new(scheduler: Box<Scheduler>, max_proc_num: usize, exit_handler: fn(Pid)) -> Self {
        ProcessManager {
            procs: new_vec_default(max_proc_num),
            scheduler: Mutex::new(scheduler),
            wait_queue: new_vec_default(max_proc_num),
            children: new_vec_default(max_proc_num),
            event_hub: Mutex::new(EventHub::new()),
            exit_handler,
        }
    }

    fn alloc_pid(&self) -> Pid {
        for (i, proc) in self.procs.iter().enumerate() {
            let mut proc_lock = proc.lock();
            if proc_lock.is_none() {
                return i;
            }
            match proc_lock.as_mut().unwrap().status {
                Status::Exited(_) => if self.wait_queue[i].lock().is_empty() {
                    *proc_lock = None;
                    return i;
                },
                _ => {},
            }
        }
        panic!("Process number exceeded");
    }

    /// Add a new process
    pub fn add(&self, context: Box<Context>) -> Pid {
        let pid = self.alloc_pid();
        *(&self.procs[pid]).lock() = Some(Process {
            id: pid,
            status: Status::Ready,
            status_after_stop: Status::Ready,
            context: Some(context),
        });
        self.scheduler.lock().insert(pid);
        pid
    }

    /// Make process `pid` time slice -= 1.
    /// Return true if time slice == 0.
    /// Called by timer interrupt handler.
    pub fn tick(&self, pid: Pid) -> bool {
        let mut event_hub = self.event_hub.lock();
        event_hub.tick();
        while let Some(event) = event_hub.pop() {
            match event {
                Event::Wakeup(pid) => self.set_status(pid, Status::Ready),
                Event::Dropped => {},
            }
        }
        self.scheduler.lock().tick(pid)
    }

    /// Set the priority of process `pid`
    pub fn set_priority(&self, pid: Pid, priority: u8) {
        self.scheduler.lock().set_priority(pid, priority);
    }

    /// Called by Processor to get a process to run.
    /// The manager first mark it `Running`,
    /// then take out and return its Context.
    pub fn run(&self, cpu_id: usize) -> (Pid, Box<Context>) {
        let mut scheduler = self.scheduler.lock();
        let pid = scheduler.select()
            .expect("failed to select a runnable process");
        scheduler.remove(pid);
        let mut proc_lock = self.procs[pid].lock();
        let mut proc = proc_lock.as_mut().expect("process not exist");;
        proc.status = Status::Running(cpu_id);
        (pid, proc.context.take().expect("context not exist"))
    }

    /// Called by Processor to finish running a process
    /// and give its context back.
    pub fn stop(&self, pid: Pid, context: Box<Context>) {
        let mut proc_lock = self.procs[pid].lock();
        let mut proc = proc_lock.as_mut().expect("process not exist");
        proc.status = proc.status_after_stop.clone();
        proc.status_after_stop = Status::Ready;
        proc.context = Some(context);
        match proc.status {
            Status::Ready => self.scheduler.lock().insert(pid),
            Status::Exited(_) => self.exit_handler(pid, proc),
            _ => {}
        }
    }

    /// Switch the status of a process.
    /// Insert/Remove it to/from scheduler if necessary.
    fn set_status(&self, pid: Pid, status: Status) {
        let mut scheduler = self.scheduler.lock();
        let mut proc_lock = self.procs[pid].lock();
        let mut proc = proc_lock.as_mut().expect("process not exist");
        trace!("process {} {:?} -> {:?}", pid, proc.status, status);
        match (&proc.status, &status) {
            (Status::Ready, Status::Ready) => return,
            (Status::Ready, _) => scheduler.remove(pid),
            (Status::Running(_), _) => {},
            (Status::Exited(_), _) => panic!("can not set status for a exited process"),
            (Status::Waiting(target), Status::Exited(_)) =>
                self.wait_queue[*target].lock().retain(|&i| i != pid),
            (Status::Sleeping, Status::Exited(_)) => self.event_hub.lock().remove(Event::Wakeup(pid)),
            (_, Status::Ready) => scheduler.insert(pid),
            _ => {}
        }
        match proc.status {
            Status::Running(_) => proc.status_after_stop = status,
            _ => proc.status = status,
        }
        match proc.status {
            Status::Exited(_) => self.exit_handler(pid, proc),
            _ => {}
        }
    }


    pub fn get_status(&self, pid: Pid) -> Option<Status> {
        let mut proc_lock = self.procs[pid].lock();
        if proc_lock.is_none() {
            return None;
        }
        match proc_lock.as_ref().unwrap().status {
            Status::Exited(_) => if self.wait_queue[pid].lock().is_empty() {
                *proc_lock = None;
                return None;
            },
            _ => {},
        }
        proc_lock.as_ref().map(|p| p.status.clone())
    }

    pub fn wait_done(&self, pid: Pid, target: Pid) {
        let mut proc_lock = self.procs[target].lock();
        let proc = proc_lock.as_ref().expect("process not exist");
        match proc.status {
            Status::Exited(_) => self.del_child(pid, target),
            _ => panic!("can not remove non-exited process"),
        }
    }

    pub fn sleep(&self, pid: Pid, time_raw: usize) {
        self.set_status(pid, Status::Sleeping);
        let time = if time_raw >= (1 << 31) {0} else {time_raw};
        if time != 0 {
            self.event_hub.lock().push(time, Event::Wakeup(pid));
        }
    }

    pub fn wakeup(&self, pid: Pid) {
        self.set_status(pid, Status::Ready);
    }

    pub fn wait(&self, pid: Pid, target: Pid) {
        self.set_status(pid, Status::Waiting(target));
        self.wait_queue[target].lock().push(pid);
    }
    pub fn wait_child(&self, pid: Pid) {
        self.set_status(pid, Status::Waiting(0));
    }

    pub fn set_parent(&self, pid: Pid, target: Pid) {
        self.wait_queue[target].lock().push(pid);
        self.children[pid].lock().push(target);
    }
    pub fn del_child(&self, pid: Pid, target: Pid) {
        self.wait_queue[target].lock().retain(|&i| i != pid);
        self.children[pid].lock().retain(|&i| i != target);
    }
    pub fn get_children(&self, pid: Pid) -> Vec<Pid>{
        self.children[pid].lock().clone()
    }

    pub fn exit(&self, pid: Pid, code: ExitCode) {
        for child in self.children[pid].lock().drain(..) {
            self.wait_queue[child].lock().retain(|&i| i != pid);
        }
        self.set_status(pid, Status::Exited(code));
    }
     /// Called when a process exit
    fn exit_handler(&self, pid: Pid, proc: &mut Process) {
        for waiter in self.wait_queue[pid].lock().iter() {
            self.wakeup(*waiter);
        }

        proc.context = None;
        (self.exit_handler)(pid);
    }
}

fn new_vec_default<T: Default>(size: usize) -> Vec<T> {
    let mut vec = Vec::new();
    vec.resize_default(size);
    vec
}
