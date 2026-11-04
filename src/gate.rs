use std::sync::{Condvar, Mutex};

pub struct Gate {
    state: Mutex<State>,
    cv: Condvar,
}

struct State {
    free: usize,
    owner_waiting: usize,
}

pub struct Permit<'a>(&'a Gate);

impl Gate {
    pub fn new(slots: usize) -> Self {
        Gate {
            state: Mutex::new(State {
                free: slots,
                owner_waiting: 0,
            }),
            cv: Condvar::new(),
        }
    }

    pub fn acquire(&self, owner: bool) -> Permit<'_> {
        let mut s = self.state.lock().unwrap();
        if owner {
            s.owner_waiting += 1;
        }
        while s.free == 0 || (!owner && s.owner_waiting > 0) {
            s = self.cv.wait(s).unwrap();
        }
        s.free -= 1;
        if owner {
            s.owner_waiting -= 1;
        }
        Permit(self)
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().free += 1;
        self.0.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn owner_goes_before_background() {
        let g = Arc::new(Gate::new(1));
        let held = g.acquire(true);
        let order = Arc::new(Mutex::new(Vec::new()));
        let bg = {
            let (g, o) = (g.clone(), order.clone());
            std::thread::spawn(move || {
                let _p = g.acquire(false);
                o.lock().unwrap().push("background");
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        let ow = {
            let (g, o) = (g.clone(), order.clone());
            std::thread::spawn(move || {
                let _p = g.acquire(true);
                o.lock().unwrap().push("owner");
                std::thread::sleep(Duration::from_millis(20));
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        drop(held);
        ow.join().unwrap();
        bg.join().unwrap();
        assert_eq!(*order.lock().unwrap(), vec!["owner", "background"]);
    }
}
