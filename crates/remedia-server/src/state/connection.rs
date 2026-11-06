use super::*;

impl ConnectionContext {
    pub(crate) fn new(id: String, principal: String, bot_user_id: u64) -> Self {
        let (active_session, _) = watch::channel(None);
        Self {
            id,
            principal,
            bot_user_id,
            active_session,
            session_control: tokio::sync::Mutex::new(()),
            probes: Mutex::new(std::collections::HashMap::new()),
            request_window: Mutex::new((Instant::now(), 0)),
        }
    }

    pub(crate) fn allows_request(&self, maximum_per_second: usize) -> bool {
        let mut window = lock(&self.request_window);
        if window.0.elapsed() >= Duration::from_secs(1) {
            *window = (Instant::now(), 0);
        }
        if window.1 >= maximum_per_second {
            return false;
        }
        window.1 += 1;
        true
    }

    pub(crate) fn session(&self) -> Option<Arc<ClientSession>> {
        self.active_session.borrow().clone()
    }

    pub(crate) fn set_session(&self, session: Option<Arc<ClientSession>>) {
        self.active_session.send_replace(session);
    }
}
