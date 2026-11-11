use super::Limits;
use super::Platform;
use super::encoded;

#[test]
fn a_service_with_limits_has_them_and_one_without_has_none() {
    let platform = Platform::decode(encoded(|p| {
        let mut services = p.init_services(2);
        let mut limited = services.reborrow().get(0);
        limited.set_name("limited");
        let mut limits = limited.init_limits();
        limits.set_cpu_ms(50);
        limits.set_subrequests(1000);
        limits.set_memory_bytes(128 << 20);
        services.get(1).set_name("plain");
    }))
    .unwrap();
    assert_eq!(
        platform.service("limited").unwrap().limits,
        Some(Limits {
            cpu_ms: 50,
            subrequests: 1000,
            memory_bytes: 128 << 20,
            durable_object_storage_bytes: 0,
        })
    );
    assert_eq!(platform.service("plain").unwrap().limits, None);
    assert!(platform.service("absent").is_none());
}

#[test]
fn a_rebuild_group_is_read_and_an_empty_one_is_none() {
    let platform = Platform::decode(encoded(|p| {
        let mut services = p.init_services(3);
        let mut first = services.reborrow().get(0);
        first.set_name("first");
        first.set_rebuild_group("group-1");
        let mut second = services.reborrow().get(1);
        second.set_name("second");
        second.set_rebuild_group("");
        services.get(2).set_name("third");
    }))
    .unwrap();
    assert_eq!(platform.rebuild_group("first"), Some("group-1"));
    assert_eq!(platform.rebuild_group("second"), None);
    assert_eq!(platform.rebuild_group("third"), None);
    assert_eq!(platform.rebuild_group("absent"), None);
}

#[test]
fn a_memory_limit_past_what_a_cage_holds_is_refused() {
    let at_most = |bytes: u64| {
        Platform::decode(encoded(|p| {
            let mut service = p.init_services(1).get(0);
            service.set_name("a");
            service.init_limits().set_memory_bytes(bytes);
        }))
    };
    assert!(at_most(super::MAX_MEMORY_BYTES).is_ok());
    assert!(at_most(super::MAX_MEMORY_BYTES + 1).is_err());
}

#[test]
fn a_service_named_twice_is_refused() {
    let words = encoded(|p| {
        let mut services = p.init_services(2);
        services.reborrow().get(0).set_name("a");
        services.get(1).set_name("a");
    });
    assert!(Platform::decode(words).is_err());
}

#[test]
fn without_settings_nothing_is_named() {
    let platform = Platform::none();
    assert!(platform.words().is_empty());
    assert!(platform.service("main").is_none());
}
