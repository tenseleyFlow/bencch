! CHECK: 14
program main
    use bridge_aliases, only: chosen => lifted
    implicit none

    print *, chosen
end program main
