use crate::agent_rt::types::{AgentPid, Message};

// ============ GenAgent (gen_server equivalent) ============

pub enum Reply {
    Ok(serde_json::Value),
    Error(String),
    Stop(String),
}

pub enum Noreply {
    Ok,
    Stop(String),
}

pub trait GenAgent: Send + Sync {
    type State: Send + 'static;

    fn init(&self, args: serde_json::Value) -> Result<Self::State, String>;

    fn handle_request(
        &self,
        state: &mut Self::State,
        from: AgentPid,
        request: serde_json::Value,
    ) -> Reply;

    fn handle_notify(
        &self,
        state: &mut Self::State,
        notification: serde_json::Value,
    ) -> Noreply;

    fn handle_info(&self, state: &mut Self::State, msg: Message) -> Noreply;

    fn code_change(
        &self,
        state: &mut Self::State,
        old_vsn: &str,
        extra: serde_json::Value,
    ) -> Result<(), String> {
        let _ = (old_vsn, extra);
        Ok(())
    }

    fn terminate(&self, state: &mut Self::State, reason: &str) {
        let _ = reason;
    }
}

/// Dispatch a JSON message to the appropriate GenAgent callback.
/// Returns Some(reply_value) for $call messages, None for $cast/$info.
pub fn dispatch_gen_agent_message<T: GenAgent>(
    agent: &T,
    state: &mut Box<dyn std::any::Any + Send>,
    msg: serde_json::Value,
) -> Option<serde_json::Value> {
    let typed_state = state.downcast_mut::<T::State>().expect("state type mismatch");

    if let Some(call_payload) = msg.get("$call") {
        let from_raw = msg
            .get("$from")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let from = AgentPid::from_raw(from_raw);
        let reply = agent.handle_request(typed_state, from, call_payload.clone());
        match reply {
            Reply::Ok(v) => Some(v),
            Reply::Error(e) => Some(serde_json::json!({"$error": e})),
            Reply::Stop(reason) => Some(serde_json::json!({"$stop": reason})),
        }
    } else if let Some(cast_payload) = msg.get("$cast") {
        agent.handle_notify(typed_state, cast_payload.clone());
        None
    } else {
        agent.handle_info(typed_state, Message::Json(msg));
        None
    }
}

// ============ StateMachine (gen_fsm equivalent) ============

pub enum Transition {
    Next { state: String },
    Same,
    Stop(String),
}

pub trait StateMachine: Send + Sync {
    type Data: Send + 'static;

    fn init(&self, args: serde_json::Value) -> Result<(String, Self::Data), String>;

    fn handle_event(
        &self,
        state_name: &str,
        data: &mut Self::Data,
        event: serde_json::Value,
    ) -> Transition;

    fn code_change(
        &self,
        state_name: &str,
        data: &mut Self::Data,
        old_vsn: &str,
    ) -> Result<(), String> {
        let _ = (state_name, old_vsn);
        Ok(())
    }
}

// ============ EventManager (gen_event equivalent) ============

pub enum EventAction {
    Ok,
    Remove,
}

pub trait EventHandler: Send + Sync {
    fn handle_event(&self, event: &serde_json::Value) -> EventAction;
    fn id(&self) -> &str;
}

pub struct EventManager {
    handlers: Vec<Box<dyn EventHandler>>,
}

impl EventManager {
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
        }
    }

    pub fn add_handler(&mut self, handler: Box<dyn EventHandler>) {
        self.handlers.push(handler);
    }

    pub fn remove_handler(&mut self, id: &str) {
        self.handlers.retain(|h| h.id() != id);
    }

    pub fn notify(&mut self, event: &serde_json::Value) {
        let mut to_remove = Vec::new();
        for (idx, handler) in self.handlers.iter().enumerate() {
            match handler.handle_event(event) {
                EventAction::Ok => {}
                EventAction::Remove => to_remove.push(idx),
            }
        }
        for idx in to_remove.into_iter().rev() {
            self.handlers.remove(idx);
        }
    }

    pub fn handler_count(&self) -> usize {
        self.handlers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_rt::types::{AgentPid, Message};

    // ---- GenAgent test implementation ----

    struct CounterAgent;

    impl GenAgent for CounterAgent {
        type State = i64;

        fn init(&self, args: serde_json::Value) -> Result<Self::State, String> {
            Ok(args.get("initial").and_then(|v| v.as_i64()).unwrap_or(0))
        }

        fn handle_request(
            &self,
            state: &mut Self::State,
            _from: AgentPid,
            request: serde_json::Value,
        ) -> Reply {
            match request.get("action").and_then(|v| v.as_str()) {
                Some("get") => Reply::Ok(serde_json::json!(*state)),
                Some("increment") => {
                    *state += 1;
                    Reply::Ok(serde_json::json!(*state))
                }
                _ => Reply::Error("unknown action".into()),
            }
        }

        fn handle_notify(
            &self,
            state: &mut Self::State,
            notification: serde_json::Value,
        ) -> Noreply {
            if let Some(val) = notification.get("set").and_then(|v| v.as_i64()) {
                *state = val;
            }
            Noreply::Ok
        }

        fn handle_info(&self, _state: &mut Self::State, _msg: Message) -> Noreply {
            Noreply::Ok
        }
    }

    #[test]
    fn test_gen_agent_init() {
        let agent = CounterAgent;
        let state = agent.init(serde_json::json!({"initial": 10})).unwrap();
        assert_eq!(state, 10);
    }

    #[test]
    fn test_gen_agent_handle_request() {
        let agent = CounterAgent;
        let mut state = 0i64;
        let from = AgentPid::new();
        let reply = agent.handle_request(
            &mut state,
            from,
            serde_json::json!({"action": "increment"}),
        );
        assert!(matches!(reply, Reply::Ok(v) if v == serde_json::json!(1)));
    }

    #[test]
    fn test_gen_agent_handle_notify() {
        let agent = CounterAgent;
        let mut state = 0i64;
        agent.handle_notify(&mut state, serde_json::json!({"set": 42}));
        assert_eq!(state, 42);
    }

    #[test]
    fn test_gen_agent_call_message_dispatch() {
        let agent = CounterAgent;
        let mut state: Box<dyn std::any::Any + Send> = Box::new(0i64);
        let from = AgentPid::new();

        let msg = serde_json::json!({
            "$call": {"action": "increment"},
            "$from": from.raw()
        });
        let reply = dispatch_gen_agent_message(&agent, &mut state, msg);
        assert!(reply.is_some());
    }

    #[test]
    fn test_gen_agent_cast_message_dispatch() {
        let agent = CounterAgent;
        let mut state: Box<dyn std::any::Any + Send> = Box::new(0i64);

        let msg = serde_json::json!({"$cast": {"set": 99}});
        let reply = dispatch_gen_agent_message(&agent, &mut state, msg);
        assert!(reply.is_none());
        let s = state.downcast_ref::<i64>().unwrap();
        assert_eq!(*s, 99);
    }

    #[test]
    fn test_gen_agent_code_change_default() {
        let agent = CounterAgent;
        let mut state = 10i64;
        let result = agent.code_change(&mut state, "v1", serde_json::json!({}));
        assert!(result.is_ok());
    }

    // ---- StateMachine tests ----

    struct TrafficLight;

    impl StateMachine for TrafficLight {
        type Data = u32; // cycle count

        fn init(&self, _args: serde_json::Value) -> Result<(String, Self::Data), String> {
            Ok(("red".into(), 0))
        }

        fn handle_event(
            &self,
            state_name: &str,
            data: &mut Self::Data,
            _event: serde_json::Value,
        ) -> Transition {
            match state_name {
                "red" => Transition::Next { state: "green".into() },
                "green" => Transition::Next { state: "yellow".into() },
                "yellow" => {
                    *data += 1;
                    Transition::Next { state: "red".into() }
                }
                _ => Transition::Stop("unknown state".into()),
            }
        }
    }

    #[test]
    fn test_state_machine_init() {
        let sm = TrafficLight;
        let (state, data) = sm.init(serde_json::json!({})).unwrap();
        assert_eq!(state, "red");
        assert_eq!(data, 0);
    }

    #[test]
    fn test_state_machine_transitions() {
        let sm = TrafficLight;
        let mut data = 0u32;
        let t1 = sm.handle_event("red", &mut data, serde_json::json!("tick"));
        assert!(matches!(t1, Transition::Next { state } if state == "green"));
        let t2 = sm.handle_event("green", &mut data, serde_json::json!("tick"));
        assert!(matches!(t2, Transition::Next { state } if state == "yellow"));
        let t3 = sm.handle_event("yellow", &mut data, serde_json::json!("tick"));
        assert!(matches!(t3, Transition::Next { state } if state == "red"));
        assert_eq!(data, 1);
    }

    #[test]
    fn test_state_machine_code_change_default() {
        let sm = TrafficLight;
        let mut data = 0u32;
        assert!(sm.code_change("red", &mut data, "v1").is_ok());
    }

    // ---- EventManager tests ----

    struct LogHandler {
        id: String,
        count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl EventHandler for LogHandler {
        fn handle_event(&self, _event: &serde_json::Value) -> EventAction {
            self.count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            EventAction::Ok
        }
        fn id(&self) -> &str {
            &self.id
        }
    }

    struct OneShotHandler {
        id: String,
    }

    impl EventHandler for OneShotHandler {
        fn handle_event(&self, _event: &serde_json::Value) -> EventAction {
            EventAction::Remove
        }
        fn id(&self) -> &str {
            &self.id
        }
    }

    #[test]
    fn test_event_manager_broadcast() {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut em = EventManager::new();
        em.add_handler(Box::new(LogHandler {
            id: "log1".into(),
            count: count.clone(),
        }));
        em.add_handler(Box::new(LogHandler {
            id: "log2".into(),
            count: count.clone(),
        }));
        em.notify(&serde_json::json!({"event": "test"}));
        assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[test]
    fn test_event_manager_remove_handler() {
        let mut em = EventManager::new();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        em.add_handler(Box::new(LogHandler {
            id: "h1".into(),
            count: count.clone(),
        }));
        assert_eq!(em.handler_count(), 1);
        em.remove_handler("h1");
        assert_eq!(em.handler_count(), 0);
    }

    #[test]
    fn test_event_manager_auto_remove() {
        let mut em = EventManager::new();
        em.add_handler(Box::new(OneShotHandler { id: "once".into() }));
        assert_eq!(em.handler_count(), 1);
        em.notify(&serde_json::json!("trigger"));
        assert_eq!(em.handler_count(), 0);
    }
}
