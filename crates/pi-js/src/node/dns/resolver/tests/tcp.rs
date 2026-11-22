use super::*;

#[tokio::test]
async fn tcp_discards_an_unrelated_frame_and_reads_the_whole_next_frame() {
    let (address, _) = udp_server(Arc::new(|query| vec![reply(query, 0, true, None)])).await;
    let listener = TcpListener::bind(address).await.unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let len = stream.read_u16().await.unwrap();
        let mut query = vec![0; usize::from(len)];
        stream.read_exact(&mut query).await.unwrap();
        let mut valid = reply(&query, 0, false, Some(Answer::A([192, 0, 2, 20])));
        let mut other = valid.clone();
        other[0] ^= 1;
        valid.resize(8000, 0);
        let mut data = vec![0, 0];
        data.extend_from_slice(&(other.len() as u16).to_be_bytes());
        data.extend_from_slice(&other);
        data.extend_from_slice(&(valid.len() as u16).to_be_bytes());
        data.extend_from_slice(&valid);
        stream.write_all(&data).await.unwrap();
    });
    assert_eq!(ask(vec![address], 1000, 1, "frames.test").await, Ok(json!(["192.0.2.20"])));
    server.await.unwrap();
}

#[tokio::test]
async fn cancelled_tcp_waiter_does_not_drop_another_query_on_the_same_stream() {
    let (address, _) = udp_server(Arc::new(|query| vec![reply(query, 0, true, None)])).await;
    let listener = TcpListener::bind(address).await.unwrap();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut replies = Vec::new();
        for _ in 0..2 {
            let len = stream.read_u16().await.unwrap();
            let mut query = vec![0; usize::from(len)];
            stream.read_exact(&mut query).await.unwrap();
            let suffix = if first_label(&query) == "keep" { 42 } else { 9 };
            let body = reply(&query, 0, false, Some(Answer::A([192, 0, 2, suffix])));
            replies.extend_from_slice(&(body.len() as u16).to_be_bytes());
            replies.extend_from_slice(&body);
        }
        trigger.cancel();
        stream.write_all(&replies).await.unwrap();
        assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
    });
    let r = resolver(vec![address], 2500, 1);
    let (cancelled, kept) = tokio::join!(
        r.resolve("drop.test", Kind::A, false, ANY, cancel),
        r.resolve("keep.test", Kind::A, false, ANY, CancellationToken::new()),
    );
    assert_eq!(cancelled, Err("ECANCELLED"));
    assert_eq!(kept, Ok(json!(["192.0.2.42"])));
    tokio::time::timeout(Duration::from_secs(2), server).await.unwrap().unwrap();
}
