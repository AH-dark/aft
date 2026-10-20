### Fixed

- Finished and recovered bash tasks no longer keep directory descriptors open
  just to cache read permissions. AFT reopens and checks each directory's identity
  when it is accessed, preserving session isolation and symlink protection.
  Adopted tasks also release their process-exit monitor when they finish, so
  retained task history does not exhaust the daemon's file-descriptor limit.
