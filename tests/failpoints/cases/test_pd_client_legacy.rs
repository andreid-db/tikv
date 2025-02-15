// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    sync::{mpsc, Arc},
    thread,
    time::Duration,
};

use grpcio::EnvBuilder;
use kvproto::{metapb::*, pdpb::GlobalConfigItem};
use pd_client::{PdClient, RegionInfo, RegionStat, RpcClient};
use security::{SecurityConfig, SecurityManager};
use test_pd::{mocker::*, util::*, Server as MockServer};
use tikv_util::{config::ReadableDuration, worker::Builder};

fn new_test_server_and_client(
    update_interval: ReadableDuration,
) -> (MockServer<Service>, RpcClient) {
    let server = MockServer::new(1);
    let eps = server.bind_addrs();
    let client = new_client_with_update_interval(eps, None, update_interval);
    (server, client)
}

macro_rules! request {
    ($client: ident => block_on($func: tt($($arg: expr),*))) => {
        (stringify!($func), {
            let client = $client.clone();
            Box::new(move || {
                let _ = futures::executor::block_on(client.$func($($arg),*));
            })
        })
    };
    ($client: ident => $func: tt($($arg: expr),*)) => {
        (stringify!($func), {
            let client = $client.clone();
            Box::new(move || {
                let _ = client.$func($($arg),*);
            })
        })
    };
}

#[test]
fn test_pd_client_deadlock() {
    let (_server, client) = new_test_server_and_client(ReadableDuration::millis(100));
    let client = Arc::new(client);
    let pd_client_reconnect_fp = "pd_client_reconnect";

    // It contains all interfaces of PdClient.
    let test_funcs: Vec<(_, Box<dyn FnOnce() + Send>)> = vec![
        request!(client => reconnect()),
        request!(client => get_cluster_id()),
        request!(client => bootstrap_cluster(Store::default(), Region::default())),
        request!(client => is_cluster_bootstrapped()),
        request!(client => alloc_id()),
        request!(client => put_store(Store::default())),
        request!(client => get_store(0)),
        request!(client => get_all_stores(false)),
        request!(client => get_cluster_config()),
        request!(client => get_region(b"")),
        request!(client => get_region_info(b"")),
        request!(client => block_on(get_region_async(b""))),
        request!(client => block_on(get_region_info_async(b""))),
        request!(client => block_on(get_region_by_id(0))),
        request!(client => block_on(region_heartbeat(0, Region::default(), Peer::default(), RegionStat::default(), None))),
        request!(client => block_on(ask_split(Region::default()))),
        request!(client => block_on(ask_batch_split(Region::default(), 1))),
        request!(client => block_on(store_heartbeat(Default::default(), None, None))),
        request!(client => block_on(report_batch_split(vec![]))),
        request!(client => scatter_region(RegionInfo::new(Region::default(), None))),
        request!(client => block_on(get_gc_safe_point())),
        request!(client => block_on(get_store_stats_async(0))),
        request!(client => get_operator(0)),
        request!(client => block_on(get_tso())),
        request!(client => load_global_config(String::default())),
    ];

    for (name, func) in test_funcs {
        fail::cfg(pd_client_reconnect_fp, "pause").unwrap();
        // Wait for the PD client thread blocking on the fail point.
        // The GLOBAL_RECONNECT_INTERVAL is 0.1s so sleeps 0.2s here.
        thread::sleep(Duration::from_millis(200));

        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            func();
            tx.send(()).unwrap();
        });
        // Only allow to reconnect once for a func.
        client.handle_reconnect(move || {
            fail::cfg(pd_client_reconnect_fp, "return").unwrap();
        });
        // Remove the fail point to let the PD client thread go on.
        fail::remove(pd_client_reconnect_fp);

        let timeout = Duration::from_millis(500);
        if rx.recv_timeout(timeout).is_err() {
            panic!("PdClient::{}() hangs", name);
        }
        handle.join().unwrap();
    }

    drop(client);
    fail::remove(pd_client_reconnect_fp);
}

#[test]
fn test_load_global_config() {
    let (mut _server, client) = new_test_server_and_client(ReadableDuration::millis(100));
    let global_items = vec![("test1", "val1"), ("test2", "val2"), ("test3", "val3")];
    let check_items = global_items.clone();
    if let Err(err) = futures::executor::block_on(
        client.store_global_config(
            String::from("global"),
            global_items
                .iter()
                .map(|(name, value)| {
                    let mut item = GlobalConfigItem::default();
                    item.set_name(name.to_string());
                    item.set_value(value.to_string());
                    item
                })
                .collect::<Vec<GlobalConfigItem>>(),
        ),
    ) {
        panic!("error occur {:?}", err);
    }

    let (res, revision) =
        futures::executor::block_on(client.load_global_config(String::from("global"))).unwrap();
    assert!(
        res.iter()
            .zip(check_items)
            .all(|(item1, item2)| item1.name == item2.0 && item1.value == item2.1)
    );
    assert_eq!(revision, 3);
}

#[test]
fn test_watch_global_config_on_closed_server() {
    let (mut server, client) = new_test_server_and_client(ReadableDuration::millis(100));
    let global_items = vec![("test1", "val1"), ("test2", "val2"), ("test3", "val3")];
    let items_clone = global_items.clone();

    let client = Arc::new(client);
    let cli_clone = client.clone();
    use futures::StreamExt;
    let background_worker = Builder::new("background").thread_count(1).create();
    background_worker.spawn_async_task(async move {
        match cli_clone.watch_global_config("global".into(), 0) {
            Ok(mut stream) => {
                let mut i: usize = 0;
                while let Some(grpc_response) = stream.next().await {
                    match grpc_response {
                        Ok(r) => {
                            for item in r.get_changes() {
                                assert_eq!(item.get_name(), items_clone[i].0);
                                assert_eq!(item.get_value(), items_clone[i].1);
                                i += 1;
                            }
                        }
                        Err(err) => panic!("failed to get stream, err: {:?}", err),
                    }
                }
            }
            Err(err) => {
                if !err.to_string().contains("UNAVAILABLE") {
                    // Not 14-UNAVAILABLE
                    panic!("other error occur {:?}", err)
                }
            }
        }
    });

    if let Err(err) = futures::executor::block_on(
        client.store_global_config(
            "global".into(),
            global_items
                .iter()
                .map(|(name, value)| {
                    let mut item = GlobalConfigItem::default();
                    item.set_name(name.to_string());
                    item.set_value(value.to_string());
                    item
                })
                .collect::<Vec<GlobalConfigItem>>(),
        ),
    ) {
        panic!("error occur {:?}", err);
    }

    thread::sleep(Duration::from_millis(100));
    server.stop();
}

// Updating pd leader may be slow, we need to make sure it does not block other
// RPC in the same gRPC Environment.
#[test]
fn test_slow_periodical_update() {
    let pd_client_reconnect_fp = "pd_client_reconnect";
    let server = MockServer::new(1);
    let eps = server.bind_addrs();

    let mut cfg = new_config(eps);
    let env = Arc::new(EnvBuilder::new().cq_count(1).build());
    let mgr = Arc::new(SecurityManager::new(&SecurityConfig::default()).unwrap());

    // client1 updates leader frequently (100ms).
    cfg.update_interval = ReadableDuration(Duration::from_millis(100));
    let _client1 = RpcClient::new(&cfg, Some(env.clone()), mgr.clone()).unwrap();

    // client2 never updates leader in the test.
    cfg.update_interval = ReadableDuration(Duration::from_secs(100));
    let client2 = RpcClient::new(&cfg, Some(env), mgr).unwrap();

    fail::cfg(pd_client_reconnect_fp, "pause").unwrap();
    // Wait for the PD client thread blocking on the fail point.
    // The GLOBAL_RECONNECT_INTERVAL is 0.1s so sleeps 0.2s here.
    thread::sleep(Duration::from_millis(200));

    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        client2.alloc_id().unwrap();
        tx.send(()).unwrap();
    });

    let timeout = Duration::from_millis(500);
    if rx.recv_timeout(timeout).is_err() {
        panic!("pd client2 is blocked");
    }

    // Clean up the fail point.
    fail::remove(pd_client_reconnect_fp);
    handle.join().unwrap();
}

// Reconnection will be speed limited.
#[test]
fn test_reconnect_limit() {
    let pd_client_reconnect_fp = "pd_client_reconnect";
    let (_server, client) = new_test_server_and_client(ReadableDuration::secs(100));

    // The GLOBAL_RECONNECT_INTERVAL is 0.1s so sleeps 0.2s here.
    thread::sleep(Duration::from_millis(200));

    // The first reconnection will succeed, and the last_update will not be updated.
    fail::cfg(pd_client_reconnect_fp, "return").unwrap();
    client.reconnect().unwrap();
    // The subsequent reconnection will be cancelled.
    for _ in 0..10 {
        let ret = client.reconnect();
        assert!(format!("{:?}", ret.unwrap_err()).contains("cancel reconnection"));
    }

    fail::remove(pd_client_reconnect_fp);
}
