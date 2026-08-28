use std::net::SocketAddr;

/// Verifies that an accepted loopback TCP connection is owned by the same
/// operating-system user as the current process.
///
/// Windows resolves the client-side endpoint through the system TCP owner
/// table, then compares process-token user SIDs. Missing, ambiguous,
/// stale, or inaccessible ownership fails closed. Other supported
/// platforms retain the existing loopback-only transport boundary.
pub fn is_tcp_peer_owned_by_current_user(
  local_addr: SocketAddr,
  peer_addr: SocketAddr,
) -> crate::Result<bool> {
  if !local_addr.ip().is_loopback() || !peer_addr.ip().is_loopback() {
    return Ok(false);
  }

  #[cfg(target_os = "windows")]
  {
    windows_impl::is_tcp_peer_owned_by_current_user(local_addr, peer_addr)
  }

  #[cfg(not(target_os = "windows"))]
  {
    Ok(true)
  }
}

#[cfg(target_os = "windows")]
mod windows_impl {
  use std::{
    mem::{size_of, size_of_val},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    slice,
    sync::OnceLock,
  };

  use windows::Win32::{
    Foundation::{
      CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE, NO_ERROR, PSID,
    },
    NetworkManagement::IpHelper::{
      GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID,
      MIB_TCP_STATE_ESTAB, TCP_TABLE_OWNER_PID_CONNECTIONS,
    },
    Security::{
      CopySid, EqualSid, GetLengthSid, GetTokenInformation, IsValidSid,
      TokenUser, TOKEN_QUERY, TOKEN_USER,
    },
    System::Threading::{
      GetCurrentProcess, OpenProcess, OpenProcessToken,
      PROCESS_QUERY_LIMITED_INFORMATION,
    },
  };

  struct OwnedHandle(HANDLE);

  impl Drop for OwnedHandle {
    fn drop(&mut self) {
      unsafe {
        _ = CloseHandle(self.0);
      }
    }
  }

  struct OwnedSid(Vec<usize>);

  impl OwnedSid {
    fn as_psid(&self) -> PSID {
      PSID(self.0.as_ptr().cast_mut().cast())
    }
  }

  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  struct TcpOwnerRow {
    local_addr: Ipv4Addr,
    local_port: u16,
    remote_addr: Ipv4Addr,
    remote_port: u16,
    owning_pid: u32,
    established: bool,
  }

  impl From<MIB_TCPROW_OWNER_PID> for TcpOwnerRow {
    fn from(row: MIB_TCPROW_OWNER_PID) -> Self {
      Self {
        local_addr: Ipv4Addr::from(u32::from_be(row.dwLocalAddr)),
        local_port: u16::from_be(row.dwLocalPort as u16),
        remote_addr: Ipv4Addr::from(u32::from_be(row.dwRemoteAddr)),
        remote_port: u16::from_be(row.dwRemotePort as u16),
        owning_pid: row.dwOwningPid,
        established: row.dwState == MIB_TCP_STATE_ESTAB.0 as u32,
      }
    }
  }

  fn tcp_owner_rows() -> crate::Result<Vec<TcpOwnerRow>> {
    const AF_INET: u32 = 2;

    let mut required_size = 0_u32;
    let first_result = unsafe {
      GetExtendedTcpTable(
        None,
        &raw mut required_size,
        false,
        AF_INET,
        TCP_TABLE_OWNER_PID_CONNECTIONS,
        0,
      )
    };
    if first_result != ERROR_INSUFFICIENT_BUFFER.0
      || required_size
        < u32::try_from(size_of::<MIB_TCPTABLE_OWNER_PID>())?
    {
      return Err(crate::Error::Platform(format!(
        "Failed to size the Windows TCP owner table (error {first_result})."
      )));
    }

    let byte_count = usize::try_from(required_size)?;
    let word_count = byte_count.div_ceil(size_of::<usize>());
    let mut table_buffer = vec![0_usize; word_count];
    let second_result = unsafe {
      GetExtendedTcpTable(
        Some(table_buffer.as_mut_ptr().cast()),
        &raw mut required_size,
        false,
        AF_INET,
        TCP_TABLE_OWNER_PID_CONNECTIONS,
        0,
      )
    };
    if second_result != NO_ERROR.0 {
      return Err(crate::Error::Platform(format!(
        "Failed to read the Windows TCP owner table (error {second_result})."
      )));
    }

    let table =
      unsafe { &*table_buffer.as_ptr().cast::<MIB_TCPTABLE_OWNER_PID>() };
    let row_count = usize::try_from(table.dwNumEntries)?;
    let rows_size = row_count
      .checked_mul(size_of::<MIB_TCPROW_OWNER_PID>())
      .and_then(|size| size.checked_add(size_of::<u32>()))
      .ok_or_else(|| {
        crate::Error::Platform(
          "Windows TCP owner table size overflowed.".to_string(),
        )
      })?;
    if rows_size > usize::try_from(required_size)?
      || rows_size > size_of_val(table_buffer.as_slice())
    {
      return Err(crate::Error::Platform(
        "Windows returned a truncated TCP owner table.".to_string(),
      ));
    }

    let first_row: *const MIB_TCPROW_OWNER_PID =
      std::ptr::addr_of!(table.table).cast();
    let rows = unsafe { slice::from_raw_parts(first_row, row_count) };
    Ok(rows.iter().copied().map(TcpOwnerRow::from).collect())
  }

  fn owner_pid_for_endpoints(
    rows: &[TcpOwnerRow],
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
  ) -> crate::Result<u32> {
    let (IpAddr::V4(server_ip), IpAddr::V4(peer_ip)) =
      (local_addr.ip(), peer_addr.ip())
    else {
      return Err(crate::Error::Platform(
        "The Windows IPC owner check requires IPv4 endpoints.".to_string(),
      ));
    };

    let mut owners = rows.iter().filter(|row| {
      row.established
        && row.local_addr == peer_ip
        && row.local_port == peer_addr.port()
        && row.remote_addr == server_ip
        && row.remote_port == local_addr.port()
    });
    let owner = owners.next().ok_or_else(|| {
      crate::Error::Platform(
        "No Windows process owns the accepted IPC endpoint.".to_string(),
      )
    })?;
    if owners.next().is_some() || owner.owning_pid == 0 {
      return Err(crate::Error::Platform(
        "Windows reported ambiguous IPC endpoint ownership.".to_string(),
      ));
    }
    Ok(owner.owning_pid)
  }

  fn process_user_sid(process_handle: HANDLE) -> crate::Result<OwnedSid> {
    let mut token_handle = HANDLE::default();
    unsafe {
      OpenProcessToken(process_handle, TOKEN_QUERY, &raw mut token_handle)
    }?;
    let token_handle = OwnedHandle(token_handle);

    let mut required_size = 0_u32;
    unsafe {
      _ = GetTokenInformation(
        token_handle.0,
        TokenUser,
        None,
        0,
        &raw mut required_size,
      );
    }
    if required_size < u32::try_from(size_of::<TOKEN_USER>())? {
      return Err(crate::Error::Platform(
        "Windows returned an empty process user token.".to_string(),
      ));
    }

    let byte_count = usize::try_from(required_size)?;
    let word_count = byte_count.div_ceil(size_of::<usize>());
    let mut token_buffer = vec![0_usize; word_count];
    unsafe {
      GetTokenInformation(
        token_handle.0,
        TokenUser,
        Some(token_buffer.as_mut_ptr().cast()),
        required_size,
        &raw mut required_size,
      )?;
    }
    let token_user =
      unsafe { &*token_buffer.as_ptr().cast::<TOKEN_USER>() };
    if !unsafe { IsValidSid(token_user.User.Sid) }.as_bool() {
      return Err(crate::Error::Platform(
        "Windows returned an invalid process user SID.".to_string(),
      ));
    }

    let sid_size = unsafe { GetLengthSid(token_user.User.Sid) };
    if sid_size == 0 {
      return Err(crate::Error::Platform(
        "Windows returned an empty process user SID.".to_string(),
      ));
    }
    let sid_words =
      usize::try_from(sid_size)?.div_ceil(size_of::<usize>());
    let mut sid_buffer = vec![0_usize; sid_words];
    unsafe {
      CopySid(
        sid_size,
        PSID(sid_buffer.as_mut_ptr().cast()),
        token_user.User.Sid,
      )?;
    }
    Ok(OwnedSid(sid_buffer))
  }

  fn current_user_sid() -> crate::Result<&'static OwnedSid> {
    static CURRENT_USER_SID: OnceLock<OwnedSid> = OnceLock::new();
    if let Some(sid) = CURRENT_USER_SID.get() {
      return Ok(sid);
    }

    let sid = process_user_sid(unsafe { GetCurrentProcess() })?;
    _ = CURRENT_USER_SID.set(sid);
    CURRENT_USER_SID.get().ok_or_else(|| {
      crate::Error::Platform(
        "Failed to cache the current process user SID.".to_string(),
      )
    })
  }

  fn process_is_owned_by_current_user(
    process_handle: HANDLE,
  ) -> crate::Result<bool> {
    let peer_sid = process_user_sid(process_handle)?;
    sid_matches_current_user(&peer_sid)
  }

  fn sid_matches_current_user(peer_sid: &OwnedSid) -> crate::Result<bool> {
    Ok(
      unsafe {
        EqualSid(current_user_sid()?.as_psid(), peer_sid.as_psid())
      }
      .is_ok(),
    )
  }

  pub(super) fn is_tcp_peer_owned_by_current_user(
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
  ) -> crate::Result<bool> {
    let rows = tcp_owner_rows()?;
    let owner_pid = owner_pid_for_endpoints(&rows, local_addr, peer_addr)?;
    let process_handle = OwnedHandle(unsafe {
      OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, owner_pid)?
    });

    // Hold the process handle while re-reading ownership. Windows does not
    // reuse that PID until the handle closes, so a client that exits
    // between the first table snapshot and token lookup cannot
    // authorize a different process that happened to receive the same
    // numeric PID.
    let confirmed_owner =
      owner_pid_for_endpoints(&tcp_owner_rows()?, local_addr, peer_addr)?;
    if confirmed_owner != owner_pid {
      return Err(crate::Error::Platform(
        "IPC endpoint ownership changed during authentication."
          .to_string(),
      ));
    }

    process_is_owned_by_current_user(process_handle.0)
  }

  #[cfg(test)]
  mod tests {
    use windows::Win32::Security::{CreateWellKnownSid, WinWorldSid};

    use super::*;

    fn row(
      local_addr: Ipv4Addr,
      local_port: u16,
      remote_addr: Ipv4Addr,
      remote_port: u16,
      owning_pid: u32,
    ) -> TcpOwnerRow {
      TcpOwnerRow {
        local_addr,
        local_port,
        remote_addr,
        remote_port,
        owning_pid,
        established: true,
      }
    }

    #[test]
    fn endpoint_lookup_selects_client_side_owner() {
      let loopback = Ipv4Addr::LOCALHOST;
      let server: SocketAddr = (loopback, 6123).into();
      let peer: SocketAddr = (loopback, 49_000).into();
      let rows = [
        row(loopback, 6123, loopback, 49_000, 100),
        row(loopback, 49_000, loopback, 6123, 200),
      ];

      assert_eq!(
        owner_pid_for_endpoints(&rows, server, peer).unwrap(),
        200
      );
    }

    #[test]
    fn missing_or_ambiguous_endpoint_owner_fails_closed() {
      let loopback = Ipv4Addr::LOCALHOST;
      let server: SocketAddr = (loopback, 6123).into();
      let peer: SocketAddr = (loopback, 49_000).into();
      assert!(owner_pid_for_endpoints(&[], server, peer).is_err());

      let duplicate = row(loopback, 49_000, loopback, 6123, 200);
      assert!(owner_pid_for_endpoints(
        &[duplicate, duplicate],
        server,
        peer
      )
      .is_err());
    }

    #[test]
    fn raw_tcp_owner_row_decodes_network_byte_order() {
      let raw = MIB_TCPROW_OWNER_PID {
        dwState: MIB_TCP_STATE_ESTAB.0 as u32,
        dwLocalAddr: u32::from_be(u32::from(Ipv4Addr::LOCALHOST)),
        dwLocalPort: u32::from(49_000_u16.to_be()),
        dwRemoteAddr: u32::from_be(u32::from(Ipv4Addr::LOCALHOST)),
        dwRemotePort: u32::from(6123_u16.to_be()),
        dwOwningPid: 42,
      };

      assert_eq!(
        TcpOwnerRow::from(raw),
        row(Ipv4Addr::LOCALHOST, 49_000, Ipv4Addr::LOCALHOST, 6123, 42)
      );
    }

    #[test]
    fn different_valid_user_sid_is_rejected() {
      let mut sid_size = 68_u32;
      let mut sid_buffer =
        vec![0_usize; 68_usize.div_ceil(size_of::<usize>())];
      unsafe {
        CreateWellKnownSid(
          WinWorldSid,
          PSID::default(),
          PSID(sid_buffer.as_mut_ptr().cast()),
          &raw mut sid_size,
        )
        .expect("the well-known World SID should be created");
      }

      assert!(!sid_matches_current_user(&OwnedSid(sid_buffer)).unwrap());
    }
  }
}
