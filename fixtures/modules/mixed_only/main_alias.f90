! CHECK: 13
program main
    use mixed_only_bridge, only: chosen => kept, beta
    implicit none

    print *, chosen + beta
end program main
