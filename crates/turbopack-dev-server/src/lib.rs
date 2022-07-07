#![feature(min_specialization)]

pub mod fs;
pub mod html;

use std::{
    collections::{HashSet, VecDeque},
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    time::Instant,
};

use anyhow::{anyhow, Result};
use hyper::{
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server,
};
use turbo_tasks::{trace::TraceRawVcs, TransientValue};
use turbo_tasks_fs::{FileContent, FileSystemPathVc};
use turbopack_core::{
    asset::AssetVc,
    reference::{all_assets, all_referenced_assets},
};

#[turbo_tasks::value(shared)]
enum FindAssetResult {
    NotFound,
    Found(AssetVc),
}

#[turbo_tasks::value(cell: new, serialization: none, eq: manual)]
pub struct DevServer {
    root_path: FileSystemPathVc,
    root_asset: AssetVc,
    #[trace_ignore]
    fallback_handler: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>,
}

#[turbo_tasks::value_impl]
impl DevServerVc {
    #[turbo_tasks::function]
    pub fn new(
        root_path: FileSystemPathVc,
        root_asset: AssetVc,
        fallback_handler: TransientValue<Arc<dyn Fn(&str) -> Option<String> + Send + Sync>>,
    ) -> Self {
        Self::cell(DevServer {
            root_path,
            root_asset,
            fallback_handler: fallback_handler.into_value(),
        })
    }
}

#[turbo_tasks::value_impl]
impl DevServerVc {
    #[turbo_tasks::function]
    async fn find_asset(self, root_asset: AssetVc, path: &str) -> Result<FindAssetResultVc> {
        let assets = all_assets(root_asset);
        let root_path = &*self.await?.root_path.await?;
        for (p, asset) in assets
            .await?
            .iter()
            .map(|asset| (asset.path(), *asset))
            .collect::<Vec<_>>()
        {
            if let Some(sub_path) = root_path.get_path_to(&*p.await?) {
                if sub_path == path {
                    return Ok(FindAssetResult::Found(asset).into());
                }
            }
        }
        Ok(FindAssetResult::NotFound.into())
    }
    #[turbo_tasks::function]
    async fn find_asset_2(self, root_asset: AssetVc, path: &str) -> Result<FindAssetResultVc> {
        let root_path = &*self.await?.root_path.await?;
        let p = &*root_asset.path().await?;
        let mut visited = HashSet::new();
        visited.insert(root_asset);
        let mut queue = VecDeque::new();
        if let Some(sub_path) = root_path.get_path_to(p) {
            if sub_path == path {
                return Ok(FindAssetResult::Found(root_asset).into());
            }
            queue.push_back(root_asset);
            while let Some(asset) = queue.pop_front() {
                let references = all_referenced_assets(asset).await?;
                for inner in references.iter() {
                    if visited.insert(*inner) {
                        let p = &*inner.path().await?;
                        if let Some(sub_path) = root_path.get_path_to(p) {
                            if sub_path == path {
                                return Ok(FindAssetResult::Found(*inner).into());
                            }
                            queue.push_back(*inner);
                        }
                    }
                }
            }
        }
        Ok(FindAssetResult::NotFound.into())
    }
}

impl DevServerVc {
    pub async fn listen(self) -> Result<DevServerListening> {
        let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
        let tt = turbo_tasks::turbo_tasks();
        let this = self.await?;
        let root_asset = this.root_asset;
        let fallback_handler = this.fallback_handler.clone();
        let make_svc = make_service_fn(move |_| {
            let tt = tt.clone();
            let fallback_handler = fallback_handler.clone();
            async move {
                let handler = move |request: Request<Body>| {
                    let start = Instant::now();
                    let tt = tt.clone();
                    let fallback_handler = fallback_handler.clone();
                    async move {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        let task_id = tt.run_once(Box::pin(async move {
                            let uri = request.uri();
                            let path = uri.path();
                            let mut asset_path = path[1..].to_string();
                            if asset_path == "" || asset_path.ends_with("/") {
                                asset_path += "index.html";
                            }
                            if let FindAssetResult::Found(asset) = &*self
                                .find_asset(root_asset, &asset_path)
                                .strongly_consistent()
                                .await?
                            {
                                if let FileContent::Content(content) =
                                    &*asset.content().strongly_consistent().await?
                                {
                                    tx.send(
                                        Response::builder()
                                            .status(200)
                                            .body(Body::from(content.content().to_vec()))?,
                                    )
                                    .map_err(|_| anyhow!("receiver dropped"))?;
                                    println!("[200] {} ({}ms)", path, start.elapsed().as_millis());
                                    return Ok(());
                                }
                            }
                            if let Some(content) = fallback_handler(path) {
                                tx.send(Response::builder().status(200).body(Body::from(content))?)
                                    .map_err(|_| anyhow!("receiver dropped"))?;
                                println!("[200] {} ({}ms)", path, start.elapsed().as_millis());
                                return Ok(());
                            }
                            tx.send(Response::builder().status(404).body(Body::empty())?)
                                .map_err(|_| anyhow!("receiver dropped"))?;
                            println!("[404] {} ({}ms)", path, start.elapsed().as_millis());
                            Ok(())
                        }));
                        loop {
                            match unsafe { tt.try_read_task_output_untracked(task_id, false)? } {
                                Ok(_) => break,
                                Err(listener) => listener.await,
                            }
                        }
                        Ok::<_, anyhow::Error>(rx.await?)
                    }
                };
                Ok::<_, anyhow::Error>(service_fn(handler))
            }
        });
        let server = Server::bind(&addr).serve(make_svc);
        println!("server listening on: {}", addr);
        Ok(DevServerListening::new(async move {
            server.await?;
            Ok(())
        }))
    }
}

#[derive(TraceRawVcs)]
pub struct DevServerListening {
    #[trace_ignore]
    pub future: Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>,
}

impl DevServerListening {
    fn new(future: impl Future<Output = Result<()>> + Send + 'static) -> Self {
        Self {
            future: Box::pin(future),
        }
    }
}

pub fn register() {
    turbo_tasks::register();
    turbo_tasks_fs::register();
    turbopack_core::register();
    include!(concat!(env!("OUT_DIR"), "/register.rs"));
}
