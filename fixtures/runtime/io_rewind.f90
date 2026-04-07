! REWIND test.
! Writes a value, rewinds the file, reads it back.
! Exercises: REWIND statement, file position reset.
!
! CHECK: 99
program test_io_rewind
    implicit none
    integer :: x

    open(10, file='/tmp/afs_rewind.dat', status='replace', action='readwrite')
    write(10, *) 99
    rewind(10)

    x = 0
    read(10, *) x
    close(10)

    print *, x
end program
