! CHECK: 17
program main
    use math_aliases, only: chosen => payload
    implicit none

    print *, chosen
end program main
