! CHECK: 42
program main
    use ops, only: add_one
    implicit none

    print *, add_one(41)
end program main
