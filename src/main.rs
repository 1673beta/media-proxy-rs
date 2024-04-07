use std::{io::Write, net::SocketAddr, sync::Arc};

use axum::{response::IntoResponse, Router};
use futures::StreamExt;
use image::{AnimationDecoder, DynamicImage, ImageDecoder};
use serde::{Deserialize, Serialize};

#[derive(Debug,Serialize,Deserialize)]
pub struct ConfigFile{
	bind_addr: String,
	timeout:u64,
	user_agent:String,
	max_size:u64,
}
#[derive(Debug, Deserialize)]
pub struct RequestParams{
	url: String,
	//#[serde(rename = "static")]
	r#static:Option<String>,
}
fn main() {
	let config_path=match std::env::var("FILES_PROXY_CONFIG_PATH"){
		Ok(path)=>{
			if path.is_empty(){
				"config.json".to_owned()
			}else{
				path
			}
		},
		Err(_)=>"config.json".to_owned()
	};
	if !std::path::Path::new(&config_path).exists(){
		let default_config=ConfigFile{
			bind_addr: "0.0.0.0:12766".to_owned(),
			timeout:1000,
			user_agent: "https://github.com/yojo-art/media-proxy-rs".to_owned(),
			max_size:256*1024*1024,
		};
		let default_config=serde_json::to_string_pretty(&default_config).unwrap();
		std::fs::File::create(&config_path).expect("create default config.json").write_all(default_config.as_bytes()).unwrap();
	}
	let config:ConfigFile=serde_json::from_reader(std::fs::File::open(&config_path).unwrap()).unwrap();

	let config=Arc::new(config);
	let rt=tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
	rt.block_on(async{
		let http_addr:SocketAddr = config.bind_addr.parse().unwrap();
		let client=reqwest::Client::new();
		let app = Router::new();
		let client0=client.clone();
		let config0=config.clone();
		let app=app.route("/",axum::routing::get(move|path,headers,parms|get_file(path,headers,client0.clone(),parms,config0.clone())));
		let app=app.route("/*path",axum::routing::get(move|path,headers,parms|get_file(path,headers,client.clone(),parms,config.clone())));
		axum::Server::bind(&http_addr).serve(app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
	});
}

async fn get_file(
	axum::extract::Path(_path):axum::extract::Path<String>,
	headers:axum::http::HeaderMap,
	client:reqwest::Client,
	axum::extract::Query(q):axum::extract::Query<RequestParams>,
	config:Arc<ConfigFile>,
)->axum::response::Response{
	let mut headers=axum::headers::HeaderMap::new();
	headers.append("X-Remote-Url",q.url.parse().unwrap());
	let req=client.get(q.url);
	let req=req.timeout(std::time::Duration::from_millis(config.timeout));
	let req=req.header("UserAgent",config.user_agent.clone());
	let resp=match req.send().await{
		Ok(resp) => resp,
		Err(e) => return (axum::http::StatusCode::BAD_REQUEST,headers,format!("{:?}",e)).into_response(),
	};
	fn add_remote_header(key:&'static str,headers:&mut axum::headers::HeaderMap,remote_headers:&reqwest::header::HeaderMap){
		for v in remote_headers.get_all(key){
			headers.append(key,String::from_utf8_lossy(v.as_bytes()).parse().unwrap());
		}
	}
	let remote_headers=resp.headers();
	add_remote_header("Content-Disposition",&mut headers,remote_headers);
	add_remote_header("Content-Type",&mut headers,remote_headers);
	headers.append("Content-Security-Policy","default-src 'none'; img-src 'self'; media-src 'self'; style-src 'unsafe-inline'".parse().unwrap());
	let len_hint=resp.content_length().unwrap_or(2048.min(config.max_size));
	if len_hint>config.max_size{
		return (axum::http::StatusCode::BAD_GATEWAY,headers).into_response()
	}
	let mut response_bytes=Vec::with_capacity(len_hint as usize);
	let mut stream=resp.bytes_stream();
	while let Some(x) = stream.next().await{
		match x{
			Ok(b)=>{
				if response_bytes.len()+b.len()>config.max_size as usize{
					return (axum::http::StatusCode::BAD_GATEWAY,headers).into_response()
				}
				response_bytes.extend_from_slice(&b);
			},
			Err(e)=>{
				return (axum::http::StatusCode::BAD_GATEWAY,headers,format!("{:?}",e)).into_response()
			}
		}
	}
	if q.r#static.is_some(){
		return encode_single(headers,response_bytes);
	}
	encode(headers,response_bytes)
}
fn resize(img:DynamicImage)->DynamicImage{
	//todo
	img
}
fn encode(mut headers: axum::http::HeaderMap,response_bytes:Vec<u8>)->axum::response::Response{
	let codec=image::guess_format(&response_bytes);
	let codec=match codec{
		Ok(codec) => codec,
		Err(e) => {
			headers.append("X-Codec-Error",format!("{:?}",e).parse().unwrap());
			return (axum::http::StatusCode::OK,headers,response_bytes).into_response();
		},
	};
	match codec{
		image::ImageFormat::Png => {
			let a=match image::codecs::png::PngDecoder::new(std::io::Cursor::new(&response_bytes)){
				Ok(a)=>a,
				Err(_)=>return encode_single(headers,response_bytes)
			};
			if !a.is_apng().unwrap(){
				return encode_single(headers,response_bytes);
			}
			let size=a.dimensions();
			match a.apng(){
				Ok(frames)=>encode_anim(headers,size,frames.into_frames()),
				Err(_)=>encode_single(headers,response_bytes)
			}
		},
		image::ImageFormat::Gif => {
			match image::codecs::gif::GifDecoder::new(std::io::Cursor::new(&response_bytes)){
				Ok(a)=>encode_anim(headers,a.dimensions(),a.into_frames()),
				Err(_)=>encode_single(headers,response_bytes)
			}
		},
		image::ImageFormat::WebP => {
			let a=match image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(&response_bytes)){
				Ok(a)=>a,
				Err(_)=>return encode_single(headers,response_bytes)
			};
			if a.has_animation(){
				encode_anim(headers,a.dimensions(),a.into_frames())
			}else{
				encode_single(headers,response_bytes)
			}
		},
		_ => {
			encode_single(headers,response_bytes)
		},
	}
}
fn encode_anim(mut headers: axum::http::HeaderMap,size:(u32,u32),frames:image::Frames)->axum::response::Response{
	let conf=webp::WebPConfig::new().unwrap();
	let mut encoder=webp::AnimEncoder::new(size.0,size.1,&conf);
	let mut image_buffer=vec![];
	{
		let mut timestamp=0;
		for frame in frames{
			if let Ok(frame)=frame{
				timestamp+=std::time::Duration::from(frame.delay()).as_millis() as i32;
				let img=image::DynamicImage::ImageRgba8(frame.into_buffer());
				let img=resize(img);
				image_buffer.push((img,timestamp));
			}
		}
	}
	for (img,timestamp) in &image_buffer{
		let aframe=webp::AnimFrame::from_image(img,*timestamp);
		if let Ok(aframe)=aframe{
			encoder.add_frame(aframe);
		}
	}
	if image_buffer.is_empty(){
		headers.append("X-Proxy-Error","NoAvailableFrames".parse().unwrap());
		return (axum::http::StatusCode::BAD_GATEWAY,headers).into_response();
	};
	let buf=encoder.encode();
	headers.remove("Content-Type");
	headers.append("Content-Type","image/webp".parse().unwrap());
	(axum::http::StatusCode::OK,headers,buf.to_vec()).into_response()
}
fn encode_single(mut headers: axum::http::HeaderMap,response_bytes:Vec<u8>)->axum::response::Response{
	let img=image::load_from_memory(&response_bytes);
	let img=match img{
		Ok(img)=>img,
		Err(e)=>{
			headers.append("X-Proxy-Error",format!("DecodeError_{:?}",e).parse().unwrap());
			return (axum::http::StatusCode::OK,headers,response_bytes).into_response();
		}
	};
	let img=resize(img);
	let mut buf=vec![];
	match img.write_to(&mut std::io::Cursor::new(&mut buf),image::ImageFormat::WebP){
		Ok(_)=>{
			headers.remove("Content-Type");
			headers.append("Content-Type","image/webp".parse().unwrap());
			(axum::http::StatusCode::OK,headers,buf).into_response()
		},
		Err(e)=>{
			headers.append("X-Proxy-Error",format!("EncodeError_{:?}",e).parse().unwrap());
			(axum::http::StatusCode::OK,headers,response_bytes).into_response()
		}
	}
}
