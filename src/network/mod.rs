use std::thread::{self,Thread,Builder};
use std::sync::mpsc::{self,channel,Receiver};
use mio::tcp::*;
use mio::*;
use mio::buf::{ByteBuf,MutByteBuf};
use std::collections::HashMap;
use std::io::{self,Read,ErrorKind};
use nom::HexDisplay;
use std::error::Error;
use mio::util::Slab;
use std::net::SocketAddr;
use std::str::FromStr;
use time::precise_time_s;

pub mod amqp;

#[cfg(feature = "splice")]
mod splice;

const SERVER: Token = Token(0);

#[derive(Debug)]
pub enum ServerOrder {
  AddServer(String,String),
  RemoveServer(usize),
  Stop
}

#[derive(Debug)]
pub enum ServerMessage {
  AddedServer(String, String, usize),
  RemovedServer(usize),
  Stopped
}

#[cfg(not(feature = "splice"))]
struct Client {
  sock:           TcpStream,
  backend:        TcpStream,
  front_buf:      Option<ByteBuf>,
  front_mut_buf:  Option<MutByteBuf>,
  back_buf:       Option<ByteBuf>,
  back_mut_buf:   Option<MutByteBuf>,
  token:          Option<Token>,
  backend_token:  Option<Token>,
  back_interest:  EventSet,
  front_interest: EventSet,
  rx_count:       usize,
  tx_count:       usize
}

#[cfg(feature = "splice")]
struct Client {
  sock:           TcpStream,
  backend:        TcpStream,
  pipe_in:        splice::Pipe,
  pipe_out:       splice::Pipe,
  data_in:        bool,
  data_out:       bool,
  token:          Option<Token>,
  backend_token:  Option<Token>,
  back_interest:  EventSet,
  front_interest: EventSet,
  rx_count:       usize,
  tx_count:       usize
}

#[cfg(not(feature = "splice"))]
impl Client {
  fn new(sock: TcpStream, backend: TcpStream) -> Option<Client> {
    Some(Client {
      sock:           sock,
      backend:        backend,
      front_buf:      None,
      front_mut_buf:  Some(ByteBuf::mut_with_capacity(2048)),
      back_buf:       None,
      back_mut_buf:   Some(ByteBuf::mut_with_capacity(2048)),
      token:          None,
      backend_token:  None,
      back_interest:  EventSet::all(),
      front_interest: EventSet::all(),
      rx_count:       0,
      tx_count:       0
    })
  }

  pub fn set_tokens(&mut self, token: Token, backend: Token) {
    self.token         = Some(token);
    self.backend_token = Some(backend);
  }

  fn writable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    //println!("in writable()");
    if let Some(mut buf) = self.back_buf.take() {
      //println!("in writable 2: back_buf contains {} bytes", buf.remaining());

      match self.sock.try_write_buf(&mut buf) {
        Ok(None) => {
          println!("client flushing buf; WOULDBLOCK");

          self.back_buf = Some(buf);
          self.front_interest.insert(EventSet::writable());
        }
        Ok(Some(r)) => {
          //FIXME what happens if not everything was written?
          //println!("FRONT [{}<-{}]: wrote {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);

          self.back_mut_buf = Some(buf.flip());
          self.tx_count = self.tx_count + r;

          //self.front_interest.insert(EventSet::readable());
          self.front_interest.remove(EventSet::writable());
          self.back_interest.insert(EventSet::readable());
        }
        Err(e) =>  println!("not implemented; client err={:?}", e),
      }
    }
    event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    Ok(())
  }

  fn readable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    let mut buf = self.front_mut_buf.take().unwrap();
    //println!("in readable(): front_mut_buf contains {} bytes", buf.remaining());

    match self.sock.try_read_buf(&mut buf) {
      Ok(None) => {
        println!("We just got readable, but were unable to read from the socket?");
      }
      Ok(Some(r)) => {
        //println!("FRONT [{}->{}]: read {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);
        self.front_interest.remove(EventSet::readable());
        self.back_interest.insert(EventSet::writable());
        self.rx_count = self.rx_count + r;
        // prepare to provide this to writable
        self.front_buf = Some(buf.flip());
      }
      Err(e) => {
        println!("not implemented; client err={:?}", e);
        //self.front_interest.remove(EventSet::readable());
      }
    };

    event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    Ok(())
  }

  fn back_writable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    if let Some(mut buf) = self.front_buf.take() {
      //println!("in back_writable 2: front_buf contains {} bytes", buf.remaining());

      match self.backend.try_write_buf(&mut buf) {
        Ok(None) => {
          println!("client flushing buf; WOULDBLOCK");

          self.front_buf = Some(buf);
          self.back_interest.insert(EventSet::writable());
        }
        Ok(Some(r)) => {
          //FIXME what happens if not everything was written?
          //println!("BACK  [{}->{}]: wrote {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);

          self.front_mut_buf = Some(buf.flip());

          self.front_interest.insert(EventSet::readable());
          self.back_interest.remove(EventSet::writable());
          self.back_interest.insert(EventSet::readable());
        }
        Err(e) =>  println!("not implemented; client err={:?}", e),
      }
    }
    event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    Ok(())
  }

  fn back_readable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    let mut buf = self.back_mut_buf.take().unwrap();
    //println!("in back_readable(): back_mut_buf contains {} bytes", buf.remaining());

    match self.backend.try_read_buf(&mut buf) {
      Ok(None) => {
        println!("We just got readable, but were unable to read from the socket?");
      }
      Ok(Some(r)) => {
        //println!("BACK  [{}<-{}]: read {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);
        self.back_interest.remove(EventSet::readable());
        self.front_interest.insert(EventSet::writable());
        // prepare to provide this to writable
        self.back_buf = Some(buf.flip());
      }
      Err(e) => {
        println!("not implemented; client err={:?}", e);
        //self.interest.remove(EventSet::readable());
      }
    };

    event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    Ok(())
  }
}

#[cfg(feature = "splice")]
impl Client {
  fn new(sock: TcpStream, backend: TcpStream) -> Option<Client> {
    if let (Some(pipe_in), Some(pipe_out)) = (splice::create_pipe(), splice::create_pipe()) {
      Some(Client {
        sock:           sock,
        backend:        backend,
        pipe_in:        pipe_in,
        pipe_out:       pipe_out,
        data_in:        false,
        data_out:       false,
        token:          None,
        backend_token:  None,
        back_interest:  EventSet::all(),
        front_interest: EventSet::all(),
        tx_count:       0,
        rx_count:       0
      })
    } else {
      None
    }
  }

  pub fn set_tokens(&mut self, token: Token, backend: Token) {
    self.token         = Some(token);
    self.backend_token = Some(backend);
  }

  fn writable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    //println!("in writable()");
    if self.data_out {
      match splice::splice_out(self.pipe_out, &self.sock) {
        None => {
          //println!("client flushing buf; WOULDBLOCK");

          self.front_interest.insert(EventSet::writable());
        }
        Some(r) => {
          //FIXME what happens if not everything was written?
          //println!("FRONT [{}<-{}]: wrote {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);

          //self.front_interest.insert(EventSet::readable());
          self.front_interest.remove(EventSet::writable());
          self.back_interest.insert(EventSet::readable());
          self.data_out = false;
          self.tx_count = self.tx_count + r;
        }
      }
      event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
      event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    }
    Ok(())
  }

  fn readable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    //println!("in readable(): front_mut_buf contains {} bytes", buf.remaining());

    match splice::splice_in(&self.sock, self.pipe_in) {
      None => {
        println!("We just got readable, but were unable to read from the socket?");
      }
      Some(r) => {
        //println!("FRONT [{}->{}]: read {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);
        self.front_interest.remove(EventSet::readable());
        self.back_interest.insert(EventSet::writable());
        self.data_in = true;
        self.rx_count = self.rx_count + r;
      }
    };

    event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    Ok(())
  }

  fn back_writable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    //println!("in back_writable 2: front_buf contains {} bytes", buf.remaining());

    if self.data_in {
      match splice::splice_out(self.pipe_in, &self.backend) {
        None => {
          //println!("client flushing buf; WOULDBLOCK");

          self.back_interest.insert(EventSet::writable());
        }
        Some(r) => {
          //FIXME what happens if not everything was written?
          //println!("BACK  [{}->{}]: wrote {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);

          self.front_interest.insert(EventSet::readable());
          self.back_interest.remove(EventSet::writable());
          self.back_interest.insert(EventSet::readable());
          self.data_in = false;
        }
      }
      event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
      event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    }
    Ok(())
  }

  fn back_readable(&mut self, event_loop: &mut EventLoop<Server>) -> io::Result<()> {
    //println!("in back_readable(): back_mut_buf contains {} bytes", buf.remaining());

    match splice::splice_in(&self.backend, self.pipe_out) {
      None => {
        println!("We just got readable, but were unable to read from the socket?");
      }
      Some(r) => {
        //println!("BACK  [{}<-{}]: read {} bytes", self.token.unwrap().as_usize(), self.backend_token.unwrap().as_usize(), r);
        self.back_interest.remove(EventSet::readable());
        self.front_interest.insert(EventSet::writable());
        self.data_out = true;
      }
    };

    event_loop.reregister(&self.backend, self.backend_token.unwrap(), self.back_interest, PollOpt::edge() | PollOpt::oneshot());
    event_loop.reregister(&self.sock, self.token.unwrap(), self.front_interest, PollOpt::edge() | PollOpt::oneshot());
    Ok(())
  }
}

pub struct Backend {
  sock:          TcpListener,
  token:         Option<Token>,
  front_address: SocketAddr,
  back_address:  SocketAddr
}

pub struct Server {
  servers:         Slab<Backend>,
  clients:         Slab<Client>,
  backend:         Slab<Token>,
  max_listeners:   usize,
  max_connections: usize,
  tx:              mpsc::Sender<ServerMessage>
}

impl Server {
  fn new(max_listeners: usize, max_connections: usize, tx: mpsc::Sender<ServerMessage>) -> Server {
    Server {
      servers:         Slab::new_starting_at(Token(0), max_listeners),
      clients:         Slab::new_starting_at(Token(max_listeners), max_connections),
      backend:         Slab::new_starting_at(Token(max_listeners+max_connections), max_connections),
      max_listeners:   max_listeners,
      max_connections: max_connections,
      tx:              tx
    }
  }

  pub fn add_server(&mut self, front: &SocketAddr, back: &SocketAddr, event_loop: &mut EventLoop<Server>) -> Option<Token> {
    if let Ok(listener) = TcpListener::bind(front) {
      let back = Backend { sock: listener, token: None, front_address: front.clone(), back_address: back.clone() };
      if let Ok(tok) = self.servers.insert(back) {
        self.servers[tok].token = Some(tok);
        event_loop.register_opt(&self.servers[tok].sock, tok, EventSet::readable(), PollOpt::level()).unwrap();
        println!("added server {:?}", tok);
        Some(tok)
      } else {
        println!("could not add listener to slab");
        None
      }
    } else {
      None
    }
  }


  //FIXME: this does not close existing connections, is that what we want?
  pub fn remove_server(&mut self, tok: Token, event_loop: &mut EventLoop<Server>) -> Option<Token>{
    println!("removing server {:?}", tok);
    if self.servers.contains(tok) {
      event_loop.deregister(&self.servers[tok].sock);
      self.servers.remove(tok);
      println!("removed server {:?}", tok);
      //self.servers[tok].sock.shutdown(Shutdown::Both);
      Some(tok)
    } else {
      None
    }
  }

  pub fn accept(&mut self, event_loop: &mut EventLoop<Server>, token: Token) {
    let accepted = self.servers[token].sock.accept();
    if let Ok(Some(sock)) = accepted {
      if let Ok(mut backend) = TcpStream::connect(&self.servers[token].back_address) {
        if let Some(client) = Client::new(sock, backend) {
          if let Ok(tok) = self.clients.insert(client) {

            if let Ok(backend_tok) = self.backend.insert(tok) {
              &self.clients[tok].set_tokens(tok, backend_tok);

              event_loop.register_opt(&self.clients[tok].sock, tok, EventSet::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
              event_loop.register_opt(&self.clients[tok].backend, backend_tok, EventSet::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
              println!("accepted client {:?}", tok);
            } else {
              println!("could not add backend to slab");
            }
          } else {
            println!("could not add client to slab");
          }
        } else {
          println!("could not create a client");
        }
      } else {
        println!("could not connect to backend");
      }
    } else {
      println!("could not accept connection: {:?}", accepted);
    }
  }
}

impl Handler for Server {
  type Timeout = usize;
  type Message = ServerOrder;

  fn ready(&mut self, event_loop: &mut EventLoop<Server>, token: Token, events: EventSet) {
    //println!("{:?} got events: {:?}", token, events);
    if events.is_readable() {
      //println!("{:?} is readable", token);
      if token.as_usize() < self.max_listeners {
        if self.servers.contains(token) {
          self.accept(event_loop, token)
        }
      } else if token.as_usize() < self.max_listeners + self.max_connections {
        if self.clients.contains(token) {
          self.clients[token].readable(event_loop);
        } else {
          println!("client {:?} was removed", token);
        }
      } else if token.as_usize() < self.max_listeners + 2 * self.max_connections {
        if self.backend.contains(token) {
          let tok = self.backend[token];
          if self.clients.contains(tok) {
            self.clients[tok].back_readable(event_loop);
          } else {
            println!("client {:?} was removed", token);
          }
        } else {
          println!("backend {:?} was removed", token);
        }
      }
      //match token {
      //  SERVER => self.server.accept(event_loop).unwrap(),
      //  i => self.server.conn_readable(event_loop, i).unwrap()
     // }
    }

    if events.is_writable() {
      //println!("{:?} is writable", token);
      if token.as_usize() < self.max_listeners {
        println!("received writable for listener {:?}, this should not happen", token);
      } else  if token.as_usize() < self.max_listeners + self.max_connections {
        if self.clients.contains(token) {
          self.clients[token].writable(event_loop);
        } else {
          println!("client {:?} was removed", token);
        }
      } else if token.as_usize() < self.max_listeners + 2 * self.max_connections {
        if self.backend.contains(token) {
          let tok = self.backend[token];
          if self.clients.contains(tok) {
            self.clients[tok].back_writable(event_loop);
          } else {
            println!("client {:?} was removed", token);
          }
        } else {
          println!("backend {:?} was removed", token);
        }
      }
      //match token {
      //  SERVER => panic!("received writable for token 0"),
        //CLIENT => self.client.writable(event_loop).unwrap(),
      //  _ => self.server.conn_writable(event_loop, token).unwrap()
      //};
    }

    if events.is_hup() {
      if token.as_usize() < self.max_listeners {
        println!("should not happen: server {:?} closed", token);
      } else if token.as_usize() < self.max_listeners + self.max_connections {
        if self.clients.contains(token) {
          println!("removing client {:?}", token);
          let back_tok = self.clients[token].backend_token.unwrap();
          {
            let sock        = &mut self.clients[token].sock;
            event_loop.deregister(sock);
            sock.shutdown(Shutdown::Both);
          };
          {
            let backend     = &mut self.clients[token].backend;
            event_loop.deregister(backend);
            backend.shutdown(Shutdown::Both);
          }
          self.clients.remove(token);
          self.backend.remove(back_tok);
        } else {
          println!("client {:?} was removed", token);
        }
      } else if token.as_usize() < self.max_listeners + 2 * self.max_connections {
        if self.backend.contains(token) {
          let tok = self.backend[token];
          if self.clients.contains(tok) {
            println!("removing client {:?}", tok);
            {
              let sock        = &mut self.clients[tok].sock;
              event_loop.deregister(sock);
              sock.shutdown(Shutdown::Both);
            }
            {
              let backend     = &mut self.clients[tok].backend;
              event_loop.deregister(backend);
              backend.shutdown(Shutdown::Both);
            }
            self.clients.remove(tok);
            self.backend.remove(token);
          } else {
            println!("client {:?} was removed", token);
          }
        } else {

          println!("backend {:?} was removed", token);
        }

      }
    }
  }

  fn notify(&mut self, event_loop: &mut EventLoop<Self>, message: Self::Message) {
    println!("notified: {:?}", message);
    match message {
      ServerOrder::AddServer(front, back) => {
        if let (Ok(front_address), Ok(back_address)) = (
          FromStr::from_str(&front), FromStr::from_str(&back)
        ) {
          if let Some(token) = self.add_server(&front_address, &back_address, event_loop) {
            self.tx.send(ServerMessage::AddedServer(front, back, token.as_usize()));
          }
        }
      },
      ServerOrder::RemoveServer(id)       => {
        if let Some(token) = self.remove_server(Token(id), event_loop) {
          self.tx.send(ServerMessage::RemovedServer(token.as_usize()));
        }
      },
      ServerOrder::Stop                   => {
        event_loop.shutdown();
      }
    }
  }

  fn timeout(&mut self, event_loop: &mut EventLoop<Self>, timeout: Self::Timeout) {
    println!("timeout");
  }

  fn interrupted(&mut self, event_loop: &mut EventLoop<Self>) {
    println!("interrupted");
  }
}

pub fn start() {
  let mut event_loop = EventLoop::new().unwrap();


  println!("listen for connections");
  //event_loop.register_opt(&listener, SERVER, EventSet::readable(), PollOpt::edge() | PollOpt::oneshot()).unwrap();
  let (tx,rx) = channel::<ServerMessage>();
  let mut s = Server::new(10, 500, tx);
  {
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1234").unwrap();
    let back: SocketAddr = FromStr::from_str("127.0.0.1:5678").unwrap();
    s.add_server(&front, &back, &mut event_loop);
  }
  {
    let front: SocketAddr = FromStr::from_str("127.0.0.1:1235").unwrap();
    let back: SocketAddr = FromStr::from_str("127.0.0.1:5678").unwrap();
    s.add_server(&front, &back, &mut event_loop);
  }
  thread::spawn(move|| {
    println!("starting event loop");
    event_loop.run(&mut s).unwrap();
    println!("ending event loop");
  });
}

pub fn start_listener(max_listeners: usize, max_connections: usize, tx: mpsc::Sender<ServerMessage>) -> (Sender<ServerOrder>,thread::JoinHandle<()>)  {
  let mut event_loop = EventLoop::new().unwrap();
  let channel = event_loop.channel();
  let notify_tx = tx.clone();
  let mut server = Server::new(max_listeners, max_connections, tx);

  let join_guard = thread::spawn(move|| {
    println!("starting event loop");
    event_loop.run(&mut server).unwrap();
    println!("ending event loop");
    notify_tx.send(ServerMessage::Stopped);
  });

  (channel, join_guard)
}


#[cfg(test)]
mod tests {
  use super::*;
  use std::net::{TcpListener, TcpStream, Shutdown};
  use std::io::{Read,Write};
  use std::{thread,str};

  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn mi() {
    thread::spawn(|| { start_server(); });
    start();
    thread::sleep_ms(300);

    let mut s1 = TcpStream::connect("127.0.0.1:1234").unwrap();
    let mut s3 = TcpStream::connect("127.0.0.1:1234").unwrap();
    thread::sleep_ms(300);
    let mut s2 = TcpStream::connect("127.0.0.1:1234").unwrap();
    s1.write(&b"hello"[..]);
    println!("s1 sent");
    s2.write(&b"pouet pouet"[..]);
    println!("s2 sent");
    thread::sleep_ms(500);

    let mut res = [0; 128];
    s1.write(&b"coucou"[..]);
    let mut sz1 = s1.read(&mut res[..]).unwrap();
    println!("s1 received {:?}", str::from_utf8(&res[..sz1]));
    assert_eq!(&res[..sz1], &b"hello END"[..]);
    s3.shutdown(Shutdown::Both);
    let sz2 = s2.read(&mut res[..]).unwrap();
    println!("s2 received {:?}", str::from_utf8(&res[..sz2]));
    assert_eq!(&res[..sz2], &b"pouet pouet END"[..]);


    thread::sleep_ms(200);
    thread::sleep_ms(200);
    sz1 = s1.read(&mut res[..]).unwrap();
    println!("s1 received again({}): {:?}", sz1, str::from_utf8(&res[..sz1]));
    assert_eq!(&res[..sz1], &b"coucou END"[..]);
    //assert!(false);
  }

  /*
  #[allow(unused_mut, unused_must_use, unused_variables)]
  #[test]
  fn concurrent() {
    use std::sync::mpsc;
    use time;
    let thread_nb = 127;

    thread::spawn(|| { start_server(); });
    start();
    thread::sleep_ms(300);

    let (tx, rx) = mpsc::channel();

    let begin = time::precise_time_s();
    for i in 0..thread_nb {
      let id = i;
      let tx = tx.clone();
      thread::Builder::new().name(id.to_string()).spawn(move || {
        let s = format!("[{}] Hello world!\n", id);
        let v: Vec<u8> = s.bytes().collect();
        if let Ok(mut conn) = TcpStream::connect("127.0.0.1:1234") {
          let mut res = [0; 128];
          for j in 0..10000 {
            conn.write(&v[..]);

            if j % 5 == 0 {
              if let Ok(sz) = conn.read(&mut res[..]) {
                //println!("[{}] received({}): {:?}", id, sz, str::from_utf8(&res[..sz]));
              } else {
                println!("failed reading");
                tx.send(());
                return;
              }
            }
          }
          tx.send(());
          return;
        } else {
          println!("failed connecting");
          tx.send(());
          return;
        }
      });
    }
    //thread::sleep_ms(5000);
    for i in 0..thread_nb {
      rx.recv();
    }
    let end = time::precise_time_s();
    println!("executed in {} seconds", end - begin);
    assert!(false);
  }
  */

  #[allow(unused_mut, unused_must_use, unused_variables)]
  fn start_server() {
    let listener = TcpListener::bind("127.0.0.1:5678").unwrap();
    fn handle_client(stream: &mut TcpStream, id: u8) {
      let mut buf = [0; 128];
      let response = b" END";
      while let Ok(sz) = stream.read(&mut buf[..]) {
        if sz > 0 {
          //println!("[{}] {:?}", id, str::from_utf8(&buf[..sz]));
          stream.write(&buf[..sz]);
          thread::sleep_ms(20);
          stream.write(&response[..]);
        }
      }
    }

    let mut count = 0;
    thread::spawn(move|| {
      for conn in listener.incoming() {
        match conn {
          Ok(mut stream) => {
            thread::spawn(move|| {
              println!("got a new client: {}", count);
              handle_client(&mut stream, count)
            });
          }
          Err(e) => { println!("connection failed"); }
        }
        count += 1;
      }
    });
  }

}