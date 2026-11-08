//! Sockets between processes of this user on this computer: Unix domain sockets, which Windows 10 1803 and later
//! also has, but which Rust's std offers there only on nightly.

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener as Listener, UnixStream as Stream};

#[cfg(unix)]
pub fn bind(path: &std::path::Path) -> std::io::Result<Listener> {
    Listener::bind(path)
}

#[cfg(unix)]
pub fn connect(path: &std::path::Path) -> std::io::Result<Stream> {
    Stream::connect(path)
}

#[cfg(windows)]
pub use windows::{Stream, bind, connect};

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::io;
    use std::os::windows::io::{AsRawSocket, FromRawSocket, OwnedSocket, RawSocket};
    use std::path::Path;

    /// An AF_UNIX stream socket. Reads, writes, timeouts, shutdown and `try_clone` work on it as on TCP, which std
    /// does through the same Winsock calls; only addresses differ, and no caller asks for one.
    pub type Stream = std::net::TcpStream;

    pub struct Listener(OwnedSocket);

    type Socket = usize;
    const INVALID_SOCKET: Socket = usize::MAX;
    const AF_UNIX: i32 = 1;
    const SOCK_STREAM: i32 = 1;
    const WSA_FLAG_OVERLAPPED: u32 = 0x01;
    const WSA_FLAG_NO_HANDLE_INHERIT: u32 = 0x80;
    const BACKLOG: i32 = 128;

    #[repr(C)]
    struct SockaddrUn {
        family: u16,
        path: [u8; 108],
    }

    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn WSAStartup(version: u16, data: *mut c_void) -> i32;
        fn WSASocketW(af: i32, kind: i32, protocol: i32, info: *const c_void, group: u32, flags: u32) -> Socket;
        fn WSAGetLastError() -> i32;
        #[link_name = "bind"]
        fn bind_socket(socket: Socket, address: *const SockaddrUn, length: i32) -> i32;
        fn listen(socket: Socket, backlog: i32) -> i32;
        fn accept(socket: Socket, address: *mut c_void, length: *mut i32) -> Socket;
        #[link_name = "connect"]
        fn connect_socket(socket: Socket, address: *const SockaddrUn, length: i32) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetHandleInformation(handle: *mut c_void, mask: u32, flags: u32) -> i32;
    }

    pub fn bind(path: &Path) -> io::Result<Listener> {
        let (socket, address) = (new_socket()?, address(path)?);
        // SAFETY: `address` lives across both calls and its size is passed with it.
        if unsafe { bind_socket(socket.as_raw_socket() as Socket, &address, size_of::<SockaddrUn>() as i32) } != 0 || unsafe { listen(socket.as_raw_socket() as Socket, BACKLOG) } != 0 {
            return Err(last_error());
        }
        Ok(Listener(socket))
    }

    pub fn connect(path: &Path) -> io::Result<Stream> {
        let (socket, address) = (new_socket()?, address(path)?);
        // SAFETY: as in `bind`.
        if unsafe { connect_socket(socket.as_raw_socket() as Socket, &address, size_of::<SockaddrUn>() as i32) } != 0 {
            return Err(last_error());
        }
        Ok(Stream::from(socket))
    }

    impl Listener {
        pub fn incoming(&self) -> impl Iterator<Item = io::Result<Stream>> + '_ {
            std::iter::repeat_with(|| self.accept())
        }

        fn accept(&self) -> io::Result<Stream> {
            // SAFETY: null address and length ask accept for no peer address.
            let socket = unsafe { accept(self.0.as_raw_socket() as Socket, std::ptr::null_mut(), std::ptr::null_mut()) };
            if socket == INVALID_SOCKET {
                return Err(last_error());
            }
            // SAFETY: accept returned a new socket that nothing else owns.
            let socket = unsafe { OwnedSocket::from_raw_socket(socket as RawSocket) };
            // An accepted socket would otherwise leak into every process the agent starts. SAFETY: a socket is a handle.
            unsafe { SetHandleInformation(socket.as_raw_socket() as *mut c_void, 1, 0) };
            Ok(Stream::from(socket))
        }
    }

    fn new_socket() -> io::Result<OwnedSocket> {
        let mut data = [0u64; 64];
        // SAFETY: `data` is larger than WSADATA. Winsock counts startups, so a second one beside std's is harmless.
        unsafe { WSAStartup(0x0202, data.as_mut_ptr().cast()) };
        // SAFETY: plain integers and a null protocol info.
        let socket = unsafe { WSASocketW(AF_UNIX, SOCK_STREAM, 0, std::ptr::null(), 0, WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT) };
        if socket == INVALID_SOCKET {
            return Err(last_error());
        }
        // SAFETY: WSASocketW returned a new socket that nothing else owns.
        Ok(unsafe { OwnedSocket::from_raw_socket(socket as RawSocket) })
    }

    fn address(path: &Path) -> io::Result<SockaddrUn> {
        let bytes = path.to_str().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket path is not UTF-8"))?.as_bytes();
        let mut address = SockaddrUn { family: AF_UNIX as u16, path: [0; 108] };
        // One byte stays for the terminating NUL.
        if bytes.len() >= address.path.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("socket path {} is longer than 107 bytes", path.display())));
        }
        address.path[..bytes.len()].copy_from_slice(bytes);
        Ok(address)
    }

    fn last_error() -> io::Error {
        // SAFETY: reads this thread's last Winsock error.
        io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
    }
}
