# Installation and recovery checks

The isolated container test passed locally on 11 September 2026 using the
installation candidate and the matching Blender source. Both images used
software graphics for this test. This is recovery evidence, not NVIDIA release
qualification or a trial by someone unfamiliar with the project.

Run the same checks against a server image and Blender image built from the
same checkout:

```sh
python3 scripts/test-installation.py SERVER_IMAGE BLENDER_IMAGE --recovery
```

The test creates a unique Compose project and dedicated disposable volumes. It
runs the bracket tutorial, downloads and verifies its STL, PNG, BLEND, and MP4,
restarts the server, and checks that the completed job is still available.
It then exercises these cases:

- Stop the services, archive the whole workspace, restore into a fresh volume,
  and restart with the same images. The completed job remains available and its
  downloaded video has the same SHA-256 as the original.
- Damage a job metadata file in the restored fixture. The service reports
  blocked render recovery while a live scene edit still succeeds.
- Fill a dedicated 64 KiB temporary filesystem. An artifact write reports an
  I/O error and leaves no final artifact. Freeing the fixture's space allows
  a new write and verified download to succeed.

The successful run emitted `INSTALLATION_OK`, `BACKUP_RESTORE_OK`,
`DAMAGED_METADATA_ISOLATION_OK`, and `DISK_PRESSURE_RECOVERY_OK`, then exited
successfully after cleanup. GitHub CI runs these checks with its freshly built
images. The test checks its project identifier before recovery operations and
removes only volumes belonging to that isolated project.

This does not establish recovery from arbitrary filesystem corruption, a
hardware failure, or incompatible image upgrades. It does not impose a storage
quota on an operator's installation; the small filesystem is a test fixture.
Keep backups on separate storage and test restoration with the image pair used
to create the saved work.
